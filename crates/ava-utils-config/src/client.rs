use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::envelope::Envelope;
use crate::error::{Error, Result};
use crate::frame::{read_frame, write_frame, write_frame_with_files};

const JITTER_RANGE: f64 = 0.2;

fn jittered(nominal: Duration, unit: f64) -> Duration {
    let factor = 1.0 - JITTER_RANGE + 2.0 * JITTER_RANGE * unit;
    let nanos = nominal.as_nanos() as f64 * factor;
    let nanos = nanos.max(1.0) as u64;
    Duration::from_nanos(nanos)
}

pub const REPLY_DEADLINE: Duration = Duration::from_secs(5);

pub fn call<T: Serialize>(socket: &Path, name: &str, body: &T) -> Result<Envelope> {
    call_with_deadline(socket, name, body, REPLY_DEADLINE)
}

pub fn call_with_deadline<T: Serialize>(
    socket: &Path,
    name: &str,
    body: &T,
    deadline: Duration,
) -> Result<Envelope> {
    send(socket, &Envelope::call(name, body)?, deadline)
}

pub fn send(socket: &Path, request: &Envelope, deadline: Duration) -> Result<Envelope> {
    let started = Instant::now();
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(deadline))?;
    write_frame(&mut stream, request)?;
    let left = deadline
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or(Error::Timeout)?;
    stream.set_read_timeout(Some(left))?;
    let reply = read_frame(&mut stream)?;
    if reply.id != request.id {
        return Err(Error::Protocol(format!(
            "reply id {} does not match request id {}",
            reply.id, request.id
        )));
    }
    reply.into_result()
}

pub fn send_with_files(
    socket: &Path,
    request: &Envelope,
    files: &[BorrowedFd<'_>],
    deadline: Duration,
) -> Result<Envelope> {
    let started = Instant::now();
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(deadline))?;
    write_frame_with_files(&mut stream, request, files)?;
    let left = deadline
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or(Error::Timeout)?;
    stream.set_read_timeout(Some(left))?;
    let reply = read_frame(&mut stream)?;
    if reply.id != request.id {
        return Err(Error::Protocol(format!(
            "reply id {} does not match request id {}",
            reply.id, request.id
        )));
    }
    reply.into_result()
}

pub fn call_retrying<T: Serialize>(
    socket: &Path,
    name: &str,
    body: &T,
    on_wait: impl FnMut(&Wait, &Error),
    sleep: impl FnMut(Duration),
) -> Result<Envelope> {
    let request = Envelope::call(name, body)?;
    retry(|| send(socket, &request, REPLY_DEADLINE), on_wait, sleep)
}

pub const HARD_ERROR_AFTER: Duration = Duration::from_secs(120);
pub const SLOW_RETRY: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    pub delay: Duration,
    pub hard_error: bool,
}

#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
    waited: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            next: Duration::from_secs(2),
            waited: Duration::ZERO,
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Iterator for Backoff {
    type Item = Wait;

    fn next(&mut self) -> Option<Wait> {
        if self.waited >= HARD_ERROR_AFTER {
            return Some(Wait {
                delay: SLOW_RETRY,
                hard_error: true,
            });
        }
        let delay = self.next;
        self.waited += delay;
        self.next *= 2;
        Some(Wait {
            delay,
            hard_error: false,
        })
    }
}

fn is_down(e: &Error) -> bool {
    matches!(e, Error::Unavailable | Error::Timeout)
}

pub fn retry<T>(
    mut op: impl FnMut() -> Result<T>,
    mut on_wait: impl FnMut(&Wait, &Error),
    mut sleep: impl FnMut(Duration),
) -> Result<T> {
    let mut backoff = Backoff::new();
    let mut rng_buf = [0u8; 8];
    loop {
        match op() {
            Err(e) if is_down(&e) => {
                let wait = backoff.next().expect("backoff never ends");
                on_wait(&wait, &e);
                getrandom::fill(&mut rng_buf).expect("getrandom failed");
                let unit = u64::from_ne_bytes(rng_buf) as f64 / u64::MAX as f64;
                sleep(jittered(wait.delay, unit));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Kind;
    use crate::testutil::TempDir;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn serve_once(
        listener: UnixListener,
        f: impl FnOnce(&mut UnixStream, Envelope) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let req = read_frame(&mut s).unwrap();
            f(&mut s, req);
        })
    }

    #[test]
    fn a_retried_call_keeps_the_same_request_id_so_the_server_can_recognise_it() {
        let dir = TempDir::new();
        let sock = dir.sock("retry");
        let listener = UnixListener::bind(&sock).unwrap();
        let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = ids.clone();
        let h = thread::spawn(move || {
            for attempt in 0..3 {
                let (mut s, _) = listener.accept().unwrap();
                let req = read_frame(&mut s).unwrap();
                seen.lock().unwrap().push(req.id.clone());
                if attempt == 2 {
                    write_frame(&mut s, &Envelope::reply(&req, &"done").unwrap()).unwrap();
                }
            }
        });
        let mut waits = 0;
        let reply = call_retrying(&sock, "work", &"x", |_, _| waits += 1, |_| {}).unwrap();
        h.join().unwrap();
        assert_eq!(reply.body_as::<String>().unwrap(), "done");
        assert_eq!(waits, 2);
        let ids = ids.lock().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|i| i == &ids[0]), "{ids:?}");
    }

    #[test]
    fn two_separate_calls_never_share_a_request_id() {
        let dir = TempDir::new();
        let sock = dir.sock("distinct");
        let listener = UnixListener::bind(&sock).unwrap();
        let h = thread::spawn(move || {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                let req = read_frame(&mut s).unwrap();
                ids.push(req.id.clone());
                write_frame(&mut s, &Envelope::reply(&req, &1u8).unwrap()).unwrap();
            }
            ids
        });
        call(&sock, "a", &()).unwrap();
        call(&sock, "a", &()).unwrap();
        let ids = h.join().unwrap();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn call_gets_the_reply() {
        let dir = TempDir::new();
        let sock = dir.sock("a");
        let h = serve_once(UnixListener::bind(&sock).unwrap(), |s, req| {
            write_frame(s, &Envelope::reply(&req, &"pong").unwrap()).unwrap();
        });
        let reply = call(&sock, "ping", &"x").unwrap();
        assert_eq!(reply.kind, Kind::Reply);
        assert_eq!(reply.body_as::<String>().unwrap(), "pong");
        h.join().unwrap();
    }

    #[test]
    fn no_socket_is_unavailable() {
        let dir = TempDir::new();
        assert!(matches!(
            call(&dir.sock("missing"), "ping", &"x"),
            Err(Error::Unavailable)
        ));
    }

    #[test]
    fn silent_server_times_out_at_the_deadline() {
        let dir = TempDir::new();
        let sock = dir.sock("a");
        let h = serve_once(UnixListener::bind(&sock).unwrap(), |_, _| {
            thread::sleep(Duration::from_millis(600));
        });
        let t = Instant::now();
        let r = call_with_deadline(&sock, "ping", &"x", Duration::from_millis(200));
        assert!(matches!(r, Err(Error::Timeout)));
        assert!(t.elapsed() < Duration::from_millis(500));
        h.join().unwrap();
    }

    #[test]
    fn remote_error_is_typed() {
        let dir = TempDir::new();
        let sock = dir.sock("a");
        let h = serve_once(UnixListener::bind(&sock).unwrap(), |s, req| {
            write_frame(s, &Envelope::error(&req, "refused", "bad input").unwrap()).unwrap();
        });
        match call(&sock, "ping", &"x") {
            Err(Error::Remote { code, .. }) => assert_eq!(code, "refused"),
            other => panic!("expected Remote, got {other:?}"),
        }
        h.join().unwrap();
    }

    #[test]
    fn reply_with_the_wrong_id_is_refused() {
        let dir = TempDir::new();
        let sock = dir.sock("a");
        let h = serve_once(UnixListener::bind(&sock).unwrap(), |s, _| {
            let other = Envelope::call("ping", &"z").unwrap();
            write_frame(s, &Envelope::reply(&other, &"pong").unwrap()).unwrap();
        });
        assert!(matches!(call(&sock, "ping", &"x"), Err(Error::Protocol(_))));
        h.join().unwrap();
    }

    #[test]
    fn backoff_doubles_then_flags_a_hard_error_and_slows_to_five_minutes() {
        let waits: Vec<Wait> = Backoff::new().take(9).collect();
        let secs: Vec<u64> = waits.iter().map(|w| w.delay.as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 64, 300, 300, 300]);
        let hard: Vec<bool> = waits.iter().map(|w| w.hard_error).collect();
        assert_eq!(
            hard,
            [false, false, false, false, false, false, true, true, true]
        );
    }

    #[test]
    fn retry_waits_through_outages_then_succeeds() {
        let mut fails = 3;
        let mut slept = Vec::new();
        let r = retry(
            || {
                if fails > 0 {
                    fails -= 1;
                    Err(Error::Unavailable)
                } else {
                    Ok("up")
                }
            },
            |_, _| {},
            |d| slept.push(d),
        );
        assert_eq!(r.unwrap(), "up");
        let nominal = [2, 4, 8];
        for (i, &d) in slept.iter().enumerate() {
            let n = Duration::from_secs(nominal[i]);
            let factor = d.as_nanos() as f64 / n.as_nanos() as f64;
            assert!(
                (0.8..=1.2).contains(&factor),
                "attempt {} slept {d:?}, factor {factor}",
                i + 1
            );
        }
    }

    #[test]
    fn retry_does_not_repeat_real_errors() {
        let mut tries = 0;
        let r: Result<()> = retry(
            || {
                tries += 1;
                Err(Error::Remote {
                    code: "refused".into(),
                    message: "no".into(),
                })
            },
            |_, _| {},
            |_| {},
        );
        assert!(matches!(r, Err(Error::Remote { .. })));
        assert_eq!(tries, 1);
    }

    #[test]
    fn jittered_stays_within_range() {
        let nominal = Duration::from_millis(100);
        let cases = [0.0, 0.5, 0.999999];
        for unit in cases {
            let d = jittered(nominal, unit);
            let factor = d.as_nanos() as f64 / nominal.as_nanos() as f64;
            assert!((0.8..=1.2).contains(&factor), "unit={unit} factor={factor}");
        }
        let d0 = jittered(nominal, 0.0);
        let d1 = jittered(nominal, 0.999999);
        let f0 = d0.as_nanos() as f64 / nominal.as_nanos() as f64;
        let f1 = d1.as_nanos() as f64 / nominal.as_nanos() as f64;
        assert!((f0 - 0.8).abs() < 0.001, "unit=0 factor={f0}");
        assert!((f1 - 1.2).abs() < 0.01, "unit=~1 factor={f1}");
    }

    #[test]
    fn jittered_never_zero() {
        let d = jittered(Duration::from_millis(1), 0.0);
        assert_ne!(d, Duration::ZERO);
    }

    #[test]
    fn nominal_delays_unchanged() {
        let waits: Vec<Wait> = Backoff::new().take(9).collect();
        let secs: Vec<u64> = waits.iter().map(|w| w.delay.as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 64, 300, 300, 300]);
        let hard: Vec<bool> = waits.iter().map(|w| w.hard_error).collect();
        assert_eq!(
            hard,
            [false, false, false, false, false, false, true, true, true]
        );
    }

    #[test]
    fn retry_sleeps_are_jittered() {
        let mut attempt = 0;
        let mut slept = Vec::new();
        let _: Result<()> = retry(
            || {
                attempt += 1;
                if attempt <= 3 {
                    Err(Error::Unavailable)
                } else {
                    Ok(())
                }
            },
            |_, _| {},
            |d| slept.push(d),
        );
        let nominal = [2, 4, 8];
        for (i, &d) in slept.iter().enumerate() {
            let n = Duration::from_secs(nominal[i]);
            let factor = d.as_nanos() as f64 / n.as_nanos() as f64;
            assert!(
                (0.8..=1.2).contains(&factor),
                "attempt {} slept {d:?}, factor {factor}",
                i + 1
            );
        }
    }
}
