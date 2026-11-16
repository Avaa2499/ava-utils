use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::client::{REPLY_DEADLINE, call_with_deadline};
use crate::error::{Error, Result};
use crate::messages::{RUN_COMMAND, RunCommand};

pub const MIN_INTERVAL: Duration = Duration::from_millis(100);

pub fn interval_from(usec: Option<&str>, pid: Option<&str>, my_pid: u32) -> Option<Duration> {
    if let Some(p) = pid
        && p.parse::<u32>().ok() != Some(my_pid)
    {
        return None;
    }
    let usec: u64 = usec?.parse().ok().filter(|u| *u > 0)?;
    Some((Duration::from_micros(usec) / 3).max(MIN_INTERVAL))
}

pub fn notify_at(target: &str, message: &str) -> Result<()> {
    let sock = UnixDatagram::unbound()?;
    match target.strip_prefix('@') {
        Some(name) => {
            let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
            sock.send_to_addr(message.as_bytes(), &addr)?;
        }
        None => {
            sock.send_to(message.as_bytes(), target)?;
        }
    }
    Ok(())
}

pub fn tick(probe: impl FnOnce() -> Result<()>, notify: impl FnOnce() -> Result<()>) -> bool {
    probe().is_ok() && notify().is_ok()
}

pub fn spawn(
    socket: PathBuf,
    interval: Duration,
    notify_target: String,
    probe_deadline: Duration,
) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            tick(
                || probe(&socket, probe_deadline),
                || notify_at(&notify_target, "WATCHDOG=1"),
            );
        }
    })
}

fn probe(socket: &Path, deadline: Duration) -> Result<()> {
    call_with_deadline(socket, RUN_COMMAND, &RunCommand { args: vec![] }, deadline).map(|_| ())
}

pub fn start(socket: &Path) -> Result<bool> {
    let interval = interval_from(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    );
    let target = std::env::var("NOTIFY_SOCKET")
        .ok()
        .filter(|t| !t.is_empty());
    match (interval, target) {
        (Some(interval), Some(target)) => {
            let _ = notify_at(&target, "READY=1");
            spawn(socket.to_path_buf(), interval, target, REPLY_DEADLINE);
            Ok(true)
        }
        (Some(_), None) => Err(Error::Protocol(
            "systemd asked for a watchdog but gave no NOTIFY_SOCKET".into(),
        )),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::server::{Handler, Listener, Outcome, serve};
    use crate::testutil::TempDir;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn the_interval_is_a_third_of_the_watchdog_time_and_only_for_this_process() {
        assert_eq!(
            interval_from(Some("60000000"), None, 7),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            interval_from(Some("60000000"), Some("7"), 7),
            Some(Duration::from_secs(20))
        );
        assert_eq!(interval_from(Some("60000000"), Some("8"), 7), None);
        assert_eq!(interval_from(None, None, 7), None);
        assert_eq!(interval_from(Some("0"), None, 7), None);
        assert_eq!(interval_from(Some("soon"), None, 7), None);
        assert_eq!(interval_from(Some("1"), None, 7), Some(MIN_INTERVAL));
    }

    #[test]
    fn messages_reach_a_path_socket() {
        let dir = TempDir::new();
        let path = dir.sock("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        notify_at(path.to_str().unwrap(), "WATCHDOG=1").unwrap();
        let mut buf = [0u8; 32];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }

    #[test]
    fn messages_reach_an_abstract_socket_written_with_a_leading_at() {
        let name = format!("au-test-{}", crate::id::new_request_id().unwrap());
        let rx = UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
            .unwrap();
        notify_at(&format!("@{name}"), "READY=1").unwrap();
        let mut buf = [0u8; 32];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn notifying_a_missing_socket_is_an_error() {
        assert!(notify_at("/definitely/not/here.sock", "WATCHDOG=1").is_err());
    }

    #[test]
    fn only_a_successful_probe_leads_to_a_ping() {
        let mut pinged = false;
        assert!(tick(
            || Ok(()),
            || {
                pinged = true;
                Ok(())
            }
        ));
        assert!(pinged);
        let mut pinged = false;
        assert!(!tick(
            || Err(Error::Timeout),
            || {
                pinged = true;
                Ok(())
            }
        ));
        assert!(
            !pinged,
            "a failed probe must not tell systemd the module is alive"
        );
    }

    struct Up;
    impl Handler for Up {
        fn handle(&self, req: &Envelope) -> Outcome {
            Outcome::Reply(Envelope::reply(req, &()).unwrap())
        }
    }

    fn recv_within(rx: &UnixDatagram, wait: Duration) -> Option<Vec<u8>> {
        rx.set_read_timeout(Some(wait)).unwrap();
        let mut buf = [0u8; 32];
        rx.recv(&mut buf).ok().map(|n| buf[..n].to_vec())
    }

    #[test]
    fn a_healthy_module_keeps_pinging_systemd() {
        let dir = TempDir::new();
        let (sock, notify) = (dir.sock("m"), dir.sock("notify"));
        let rx = UnixDatagram::bind(&notify).unwrap();
        let listener = Listener::bind(&sock).unwrap();
        thread::spawn(move || serve(listener, Arc::new(Up)));
        spawn(
            sock,
            Duration::from_millis(100),
            notify.to_string_lossy().into_owned(),
            Duration::from_millis(500),
        );
        for _ in 0..3 {
            assert_eq!(
                recv_within(&rx, Duration::from_secs(3)).as_deref(),
                Some(&b"WATCHDOG=1"[..])
            );
        }
    }

    #[test]
    fn a_module_that_stops_answering_stops_pinging_so_systemd_restarts_it() {
        let dir = TempDir::new();
        let (sock, notify) = (dir.sock("m"), dir.sock("notify"));
        let rx = UnixDatagram::bind(&notify).unwrap();
        let _hung = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        spawn(
            sock,
            Duration::from_millis(100),
            notify.to_string_lossy().into_owned(),
            Duration::from_millis(200),
        );
        let started = Instant::now();
        assert_eq!(recv_within(&rx, Duration::from_millis(1500)), None);
        assert!(started.elapsed() >= Duration::from_millis(1400));
    }

    #[test]
    fn start_does_nothing_without_a_watchdog_from_systemd() {
        if std::env::var_os("WATCHDOG_USEC").is_none() {
            assert!(!start(Path::new("/nonexistent.sock")).unwrap());
        }
    }
}
