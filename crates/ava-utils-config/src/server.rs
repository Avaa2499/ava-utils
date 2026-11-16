use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use std::hash::{DefaultHasher, Hash, Hasher};

use crate::client::REPLY_DEADLINE;
use crate::dedup::ReplyCache;
use crate::envelope::{Envelope, Kind};
use crate::error::{Error, Result};
use crate::frame::{read_frame, write_frame};
use crate::messages::{CommandOutput, JOB_CANCEL, JOB_PROGRESS, RUN_COMMAND, RunCommand};
use crate::quarantine::Guard;
use crate::topic::Publisher;

pub const MAX_CONNECTIONS: usize = 256;
const SYSTEMD_FIRST_FD: i32 = 3;

pub struct Listener(UnixListener);

impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(Self(UnixListener::bind(path)?))
    }

    pub fn from_systemd() -> Result<Self> {
        let pid_ok = std::env::var("LISTEN_PID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            == Some(std::process::id());
        let fds: i32 = std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if !pid_ok || fds < 1 {
            return Err(Error::Protocol(
                "not started by systemd socket activation".into(),
            ));
        }
        Ok(Self(unsafe { UnixListener::from_raw_fd(SYSTEMD_FIRST_FD) }))
    }
}

pub enum Outcome {
    Reply(Envelope),
    Subscribe(Arc<Publisher>),
}

pub trait Handler: Send + Sync + 'static {
    fn handle(&self, request: &Envelope) -> Outcome;
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Policy {
    guard: Option<Arc<Guard>>,
    cache: ReplyCache,
}

pub fn serve<H: Handler>(listener: Listener, handler: Arc<H>) {
    serve_with(listener, handler, None)
}

pub fn serve_module<H: Handler>(module: &str, handler: Arc<H>) -> Result<()> {
    let dirs = crate::paths::Dirs::from_env()?;
    let guard = Guard::open(&dirs.state(module)?)?;
    let listener = Listener::from_systemd()?;
    let watchdog = crate::watchdog::start(&dirs.socket(module)?)?;
    eprintln!(
        "{module}: up, pid {}{}",
        std::process::id(),
        if watchdog { ", watchdog on" } else { "" }
    );
    serve_guarded(listener, handler, guard);
    Ok(())
}

pub fn serve_guarded<H: Handler>(listener: Listener, handler: Arc<H>, guard: Arc<Guard>) {
    serve_with(listener, handler, Some(guard))
}

fn serve_with<H: Handler>(listener: Listener, handler: Arc<H>, guard: Option<Arc<Guard>>) {
    let policy = Arc::new(Policy {
        guard,
        cache: ReplyCache::default(),
    });
    let open = Arc::new(AtomicUsize::new(0));
    for conn in listener.0.incoming() {
        let Ok(stream) = conn else { continue };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let slot = Slot(open.clone());
        let handler = handler.clone();
        let policy = policy.clone();
        thread::spawn(move || {
            let _slot = slot;
            let _ = handle_connection(stream, handler.as_ref(), &policy);
        });
    }
}

pub fn request_key(req: &Envelope) -> Option<(String, String)> {
    if req.name == JOB_PROGRESS || req.name == JOB_CANCEL {
        return None;
    }
    let label = if req.name == RUN_COMMAND {
        let c = req.body_as::<RunCommand>().ok()?;
        if c.args.is_empty() || c.args[0] == "quarantine" {
            return None;
        }
        c.args.join(" ")
    } else {
        format!("{} {}", req.name, req.body)
    };
    let mut h = DefaultHasher::new();
    req.name.hash(&mut h);
    let mut body = Vec::new();
    rmpv::encode::write_value(&mut body, &req.body).ok()?;
    body.hash(&mut h);
    Some((format!("{:016x}", h.finish()), label))
}

fn quarantine_command(guard: &Guard, req: &Envelope) -> Option<Result<Envelope>> {
    if req.name != RUN_COMMAND {
        return None;
    }
    let args = req.body_as::<RunCommand>().ok()?.args;
    if args.first().map(String::as_str) != Some("quarantine") {
        return None;
    }
    let text = match args.get(1).map(String::as_str) {
        None | Some("list") => {
            let all = guard.strikes();
            if all.is_empty() {
                "nothing is quarantined".to_string()
            } else {
                all.iter()
                    .map(|(label, n)| {
                        let state = if *n >= crate::quarantine::MAX_STRIKES {
                            "REFUSED"
                        } else {
                            "watching"
                        };
                        format!("{state}: {label} ({n} crashes)")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Some("clear") => {
            let needle = args[2..].join(" ");
            if needle.is_empty() {
                return Some(Envelope::error(
                    req,
                    crate::error::codes::BAD_REQUEST,
                    "usage: quarantine clear <text from the list, or all>",
                ));
            }
            match guard.clear(&needle) {
                Ok(n) => format!("cleared {n}"),
                Err(e) => return Some(Envelope::from_error(req, &e)),
            }
        }
        Some(other) => {
            return Some(Envelope::error(
                req,
                crate::error::codes::BAD_REQUEST,
                &format!("unknown quarantine command: {other} (use list or clear)"),
            ));
        }
    };
    Some(Envelope::reply(req, &CommandOutput { text, job: None }))
}

fn handle_connection(
    mut stream: UnixStream,
    handler: &impl Handler,
    policy: &Policy,
) -> Result<()> {
    stream.set_read_timeout(Some(REPLY_DEADLINE))?;
    stream.set_write_timeout(Some(REPLY_DEADLINE))?;
    let request = read_frame(&mut stream)?;
    if request.kind != Kind::Call {
        let e = Envelope::error(
            &request,
            crate::error::codes::NOT_A_CALL,
            "only calls are accepted here",
        )?;
        return write_frame(&mut stream, &e);
    }
    if let Some(guard) = &policy.guard
        && let Some(reply) = quarantine_command(guard, &request)
    {
        return write_frame(&mut stream, &reply?);
    }
    let tracked = request_key(&request);
    if tracked.is_some()
        && let Some(cached) = policy.cache.get(&request.id)
    {
        return write_frame(&mut stream, &cached);
    }
    let ticket = match (&policy.guard, &tracked) {
        (Some(g), Some((key, label))) => match g.begin(key, label) {
            Ok(t) => Some(t),
            Err(e) => return write_frame(&mut stream, &Envelope::from_error(&request, &e)?),
        },
        _ => None,
    };
    let outcome = handler.handle(&request);
    drop(ticket);
    match outcome {
        Outcome::Reply(reply) => {
            if tracked.is_some() && reply.kind == Kind::Reply {
                policy.cache.put(&request.id, &reply);
            }
            write_frame(&mut stream, &reply)
        }
        Outcome::Subscribe(publisher) => {
            stream.set_read_timeout(None)?;
            publisher.attach(&request, stream)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::call;
    use crate::testutil::TempDir;
    use crate::topic::subscribe;
    use std::time::Duration;

    struct Echo {
        topic: Arc<Publisher>,
    }

    impl Handler for Echo {
        fn handle(&self, req: &Envelope) -> Outcome {
            match req.name.as_str() {
                "echo" => {
                    Outcome::Reply(Envelope::reply(req, &req.body_as::<String>().unwrap()).unwrap())
                }
                "files-changed" => Outcome::Subscribe(self.topic.clone()),
                _ => Outcome::Reply(Envelope::error(req, "unknown-call", &req.name).unwrap()),
            }
        }
    }

    fn start(dir: &TempDir) -> (std::path::PathBuf, Arc<Publisher>) {
        let sock = dir.sock("m");
        let topic = Publisher::new("files-changed", &0u32).unwrap();
        let handler = Arc::new(Echo {
            topic: topic.clone(),
        });
        let listener = Listener::bind(&sock).unwrap();
        thread::spawn(move || serve(listener, handler));
        (sock, topic)
    }

    fn raw(sock: &std::path::Path, req: &Envelope) -> Result<Envelope> {
        let mut s = UnixStream::connect(sock)?;
        write_frame(&mut s, req)?;
        read_frame(&mut s)
    }

    struct Counting {
        n: AtomicUsize,
    }

    impl Handler for Counting {
        fn handle(&self, req: &Envelope) -> Outcome {
            match req.name.as_str() {
                "work" => {
                    let n = self.n.fetch_add(1, Ordering::SeqCst) + 1;
                    Outcome::Reply(Envelope::reply(req, &n).unwrap())
                }
                "fail" => {
                    self.n.fetch_add(1, Ordering::SeqCst);
                    Outcome::Reply(Envelope::error(req, "failed", "no").unwrap())
                }
                "boom" => panic!("handler bug"),
                _ => Outcome::Reply(Envelope::reply(req, &0u8).unwrap()),
            }
        }
    }

    fn start_counting(
        dir: &TempDir,
        guard: Option<Arc<Guard>>,
    ) -> (std::path::PathBuf, Arc<Counting>) {
        let sock = dir.sock("c");
        let h = Arc::new(Counting {
            n: AtomicUsize::new(0),
        });
        let listener = Listener::bind(&sock).unwrap();
        let h2 = h.clone();
        thread::spawn(move || match guard {
            Some(g) => serve_guarded(listener, h2, g),
            None => serve(listener, h2),
        });
        (sock, h)
    }

    fn call_named(name: &str) -> Envelope {
        Envelope::call(name, &()).unwrap()
    }

    fn crash(dir: &std::path::Path, req: &Envelope) {
        let g = Guard::open(dir).unwrap();
        let (k, l) = request_key(req).unwrap();
        std::mem::forget(g.begin(&k, &l).unwrap());
    }

    #[test]
    fn a_retry_with_the_same_request_id_gets_the_first_answer_without_running_again() {
        let dir = TempDir::new();
        let (sock, h) = start_counting(&dir, None);
        let req = call_named("work");
        let a = raw(&sock, &req).unwrap();
        let b = raw(&sock, &req).unwrap();
        assert_eq!(a, b);
        assert_eq!(h.n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn separate_requests_with_different_ids_each_run() {
        let dir = TempDir::new();
        let (sock, h) = start_counting(&dir, None);
        raw(&sock, &call_named("work")).unwrap();
        raw(&sock, &call_named("work")).unwrap();
        assert_eq!(h.n.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_requests_are_not_remembered_so_a_retry_really_retries() {
        let dir = TempDir::new();
        let (sock, h) = start_counting(&dir, None);
        let req = call_named("fail");
        assert_eq!(raw(&sock, &req).unwrap().kind, Kind::Error);
        assert_eq!(raw(&sock, &req).unwrap().kind, Kind::Error);
        assert_eq!(h.n.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn probes_and_job_controls_are_never_tracked_or_cached() {
        let probe = Envelope::call(RUN_COMMAND, &RunCommand { args: vec![] }).unwrap();
        let q = Envelope::call(
            RUN_COMMAND,
            &RunCommand {
                args: vec!["quarantine".into(), "list".into()],
            },
        )
        .unwrap();
        assert!(request_key(&probe).is_none());
        assert!(request_key(&q).is_none());
        assert!(request_key(&call_named(JOB_PROGRESS)).is_none());
        assert!(request_key(&call_named(JOB_CANCEL)).is_none());
        let scan = Envelope::call(
            RUN_COMMAND,
            &RunCommand {
                args: vec!["scan".into(), "/a".into()],
            },
        )
        .unwrap();
        let (_, label) = request_key(&scan).unwrap();
        assert_eq!(label, "scan /a");
    }

    #[test]
    fn the_same_request_has_the_same_key_and_different_input_a_different_one() {
        let mk = |args: &[&str]| {
            let c = RunCommand {
                args: args.iter().map(|s| s.to_string()).collect(),
            };
            request_key(&Envelope::call(RUN_COMMAND, &c).unwrap())
                .unwrap()
                .0
        };
        assert_eq!(mk(&["scan", "/a"]), mk(&["scan", "/a"]));
        assert_ne!(mk(&["scan", "/a"]), mk(&["scan", "/b"]));
        assert_ne!(mk(&["scan", "/a"]), mk(&["resume", "/a"]));
    }

    #[test]
    fn a_request_that_crashed_the_module_three_times_is_refused_until_cleared() {
        let dir = TempDir::new();
        let state = dir.path().join("state");
        let req = call_named("work");
        for _ in 0..3 {
            crash(&state, &req);
        }
        let (sock, h) = start_counting(&dir, Some(Guard::open(&state).unwrap()));
        match raw(&sock, &call_named("work")).unwrap().into_result() {
            Err(Error::Remote { code, message }) => {
                assert_eq!(code, crate::error::codes::REFUSED);
                assert!(message.contains("work"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            h.n.load(Ordering::SeqCst),
            0,
            "a refused request must not reach the handler"
        );
        assert!(
            raw(&sock, &call_named("other"))
                .unwrap()
                .into_result()
                .is_ok()
        );

        let cmd = |args: &[&str]| {
            let c = RunCommand {
                args: args.iter().map(|s| s.to_string()).collect(),
            };
            raw(&sock, &Envelope::call(RUN_COMMAND, &c).unwrap())
                .unwrap()
                .into_result()
                .map(|r| r.body_as::<CommandOutput>().unwrap().text)
        };
        assert!(
            cmd(&["quarantine", "list"])
                .unwrap()
                .contains("REFUSED: work")
        );
        assert!(matches!(
            cmd(&["quarantine", "clear"]),
            Err(Error::Remote { .. })
        ));
        assert!(matches!(
            cmd(&["quarantine", "bogus"]),
            Err(Error::Remote { .. })
        ));
        assert_eq!(cmd(&["quarantine", "clear", "work"]).unwrap(), "cleared 1");
        assert_eq!(
            cmd(&["quarantine", "list"]).unwrap(),
            "nothing is quarantined"
        );
        assert!(
            raw(&sock, &call_named("work"))
                .unwrap()
                .into_result()
                .is_ok()
        );
    }

    #[test]
    fn a_handler_that_panics_three_times_on_the_same_request_gets_it_refused() {
        let dir = TempDir::new();
        let state = dir.path().join("state");
        let (sock, _h) = start_counting(&dir, Some(Guard::open(&state).unwrap()));
        let bad = |s: &std::path::Path| raw(s, &call_named("boom"));
        for _ in 0..3 {
            assert!(bad(&sock).is_err());
        }
        std::thread::sleep(Duration::from_millis(50));
        match bad(&sock).unwrap().into_result() {
            Err(Error::Remote { code, .. }) => assert_eq!(code, crate::error::codes::REFUSED),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            raw(&sock, &call_named("work"))
                .unwrap()
                .into_result()
                .is_ok()
        );
    }

    #[test]
    fn a_subscription_does_not_stay_marked_as_work_in_flight() {
        let dir = TempDir::new();
        let state = dir.path().join("state");
        let sock = dir.sock("sub");
        let topic = Publisher::new("files-changed", &0u32).unwrap();
        let listener = Listener::bind(&sock).unwrap();
        let guard = Guard::open(&state).unwrap();
        let handler = Arc::new(Echo { topic });
        thread::spawn(move || serve_guarded(listener, handler, guard));
        let _sub = subscribe(&sock, "files-changed").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read_to_string(state.join("inflight.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_quarantine_command_on_an_unguarded_server_is_just_a_normal_request() {
        let dir = TempDir::new();
        let (sock, _h) = start_counting(&dir, None);
        let c = RunCommand {
            args: vec!["quarantine".into(), "list".into()],
        };
        let r = raw(&sock, &Envelope::call(RUN_COMMAND, &c).unwrap()).unwrap();
        assert_eq!(r.kind, Kind::Reply);
        assert_eq!(r.body_as::<u8>().unwrap(), 0);
    }

    #[test]
    fn answers_calls() {
        let dir = TempDir::new();
        let (sock, _) = start(&dir);
        let r = call(&sock, "echo", &"hi").unwrap();
        assert_eq!(r.body_as::<String>().unwrap(), "hi");
    }

    #[test]
    fn unknown_calls_get_a_typed_error() {
        let dir = TempDir::new();
        let (sock, _) = start(&dir);
        match call(&sock, "nope", &"x") {
            Err(Error::Remote { code, .. }) => assert_eq!(code, "unknown-call"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn subscriptions_hand_the_connection_to_the_topic() {
        let dir = TempDir::new();
        let (sock, topic) = start(&dir);
        let mut sub = subscribe(&sock, "files-changed").unwrap();
        while topic.subscribers() == 0 {
            thread::sleep(Duration::from_millis(5));
        }
        topic.publish(&5u32).unwrap();
        let e = sub.next_event(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(e.body_as::<u32>().unwrap(), 5);
    }

    #[test]
    fn a_connection_that_sends_garbage_does_not_stop_the_server() {
        use std::io::Write;
        let dir = TempDir::new();
        let (sock, _) = start(&dir);
        let mut bad = UnixStream::connect(&sock).unwrap();
        bad.write_all(&[0, 0, 0, 3, 0xc1, 0xc1, 0xc1]).unwrap();
        drop(bad);
        let r = call(&sock, "echo", &"still up").unwrap();
        assert_eq!(r.body_as::<String>().unwrap(), "still up");
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let dir = TempDir::new();
        let sock = dir.sock("m");
        std::fs::write(&sock, b"old").unwrap();
        assert!(Listener::bind(&sock).is_ok());
    }

    #[test]
    fn systemd_listener_refuses_to_start_without_activation() {
        assert!(matches!(Listener::from_systemd(), Err(Error::Protocol(_))));
    }
}
