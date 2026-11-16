use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crate::client::REPLY_DEADLINE;
use crate::envelope::{Envelope, Kind};
use crate::error::{Error, Result};
use crate::frame::{read_frame, write_frame};
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

pub fn serve<H: Handler>(listener: Listener, handler: Arc<H>) {
    let open = Arc::new(AtomicUsize::new(0));
    for conn in listener.0.incoming() {
        let Ok(stream) = conn else { continue };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let slot = Slot(open.clone());
        let handler = handler.clone();
        thread::spawn(move || {
            let _slot = slot;
            let _ = handle_connection(stream, handler.as_ref());
        });
    }
}

fn handle_connection(mut stream: UnixStream, handler: &impl Handler) -> Result<()> {
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
    match handler.handle(&request) {
        Outcome::Reply(reply) => write_frame(&mut stream, &reply),
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
