use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use rmpv::Value;
use serde::Serialize;

use crate::envelope::{Envelope, Kind};
use crate::error::{Error, Result};
use crate::frame::{read_frame, write_frame};
use crate::id::new_request_id;

pub const DEFAULT_QUEUE: usize = 1024;
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

struct Sub {
    tx: SyncSender<Envelope>,
    stream: UnixStream,
}

struct Inner {
    state: Value,
    subs: Vec<Sub>,
}

pub struct Publisher {
    topic: String,
    queue: usize,
    write_timeout: Duration,
    inner: Mutex<Inner>,
}

fn to_value<T: Serialize>(body: &T) -> Result<Value> {
    Envelope::event("x", body).map(|e| e.body)
}

impl Publisher {
    pub fn new<T: Serialize>(topic: &str, state: &T) -> Result<Arc<Self>> {
        Self::with_limits(topic, state, DEFAULT_QUEUE, DEFAULT_WRITE_TIMEOUT)
    }

    pub fn with_limits<T: Serialize>(
        topic: &str,
        state: &T,
        queue: usize,
        write_timeout: Duration,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            topic: topic.to_string(),
            queue: queue.max(2),
            write_timeout,
            inner: Mutex::new(Inner {
                state: to_value(state)?,
                subs: Vec::new(),
            }),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn subscribers(&self) -> usize {
        self.lock().subs.len()
    }

    pub fn set_state<T: Serialize>(&self, state: &T) -> Result<()> {
        self.lock().state = to_value(state)?;
        Ok(())
    }

    pub fn publish<T: Serialize>(&self, event: &T) -> Result<()> {
        let env = Envelope {
            id: new_request_id()?,
            kind: Kind::Event,
            name: self.topic.clone(),
            body: to_value(event)?,
        };
        let mut inner = self.lock();
        inner.subs.retain(|sub| match sub.tx.try_send(env.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                let _ = sub.stream.shutdown(Shutdown::Both);
                false
            }
        });
        Ok(())
    }

    pub fn attach(&self, request: &Envelope, stream: UnixStream) -> Result<()> {
        stream.set_write_timeout(Some(self.write_timeout))?;
        let (tx, rx) = sync_channel(self.queue);
        let kill = stream.try_clone()?;
        {
            let mut inner = self.lock();
            let first = Envelope {
                id: request.id.clone(),
                kind: Kind::Reply,
                name: request.name.clone(),
                body: inner.state.clone(),
            };
            tx.try_send(first)
                .map_err(|_| Error::Protocol("subscriber queue refused the state".into()))?;
            inner.subs.push(Sub { tx, stream: kill });
        }
        thread::spawn(move || write_loop(stream, rx));
        Ok(())
    }
}

fn write_loop(mut stream: UnixStream, rx: Receiver<Envelope>) {
    while let Ok(env) = rx.recv() {
        if write_frame(&mut stream, &env).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub struct Subscription {
    stream: UnixStream,
    pub state: Envelope,
}

pub fn subscribe(socket: &Path, topic: &str) -> Result<Subscription> {
    let request = Envelope::call(topic, &())?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(crate::client::REPLY_DEADLINE))?;
    write_frame(&mut stream, &request)?;
    stream.set_read_timeout(Some(crate::client::REPLY_DEADLINE))?;
    let state = read_frame(&mut stream)?;
    if state.id != request.id {
        return Err(Error::Protocol("subscribe reply id mismatch".into()));
    }
    let state = state.into_result()?;
    stream.set_read_timeout(None)?;
    Ok(Subscription { stream, state })
}

impl Subscription {
    pub fn next_event(&mut self, wait: Option<Duration>) -> Result<Envelope> {
        self.stream.set_read_timeout(wait)?;
        read_frame(&mut self.stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn accept_subscribers(listener: UnixListener, publisher: Arc<Publisher>) {
        thread::spawn(move || {
            for conn in listener.incoming() {
                let mut s = conn.unwrap();
                let req = read_frame(&mut s).unwrap();
                publisher.attach(&req, s).unwrap();
            }
        });
    }

    fn setup(
        queue: usize,
        write_timeout: Duration,
    ) -> (TempDir, std::path::PathBuf, Arc<Publisher>) {
        let dir = TempDir::new();
        let sock = dir.sock("t");
        let p = Publisher::with_limits("files-changed", &0u32, queue, write_timeout).unwrap();
        accept_subscribers(UnixListener::bind(&sock).unwrap(), p.clone());
        (dir, sock, p)
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn subscriber_gets_current_state_first_then_events_in_order() {
        let (_d, sock, p) = setup(16, Duration::from_secs(5));
        p.set_state(&7u32).unwrap();
        let mut sub = subscribe(&sock, "files-changed").unwrap();
        assert_eq!(sub.state.body_as::<u32>().unwrap(), 7);
        wait_for(|| p.subscribers() == 1);
        p.publish(&1u32).unwrap();
        p.publish(&2u32).unwrap();
        let t = Some(Duration::from_secs(2));
        assert_eq!(sub.next_event(t).unwrap().body_as::<u32>().unwrap(), 1);
        assert_eq!(sub.next_event(t).unwrap().body_as::<u32>().unwrap(), 2);
    }

    #[test]
    fn a_stuck_subscriber_is_dropped_and_never_blocks_the_producer() {
        let (_d, sock, p) = setup(4, Duration::from_secs(30));
        let _stuck = subscribe(&sock, "files-changed").unwrap();
        let mut healthy = subscribe(&sock, "files-changed").unwrap();
        wait_for(|| p.subscribers() == 2);
        let big = vec![0u8; 256 * 1024];
        let t = Instant::now();
        for _ in 0..20 {
            p.publish(&big).unwrap();
            let _ = healthy.next_event(Some(Duration::from_millis(50)));
        }
        assert!(t.elapsed() < Duration::from_secs(5));
        wait_for(|| p.subscribers() == 1);
    }

    #[test]
    fn reconnecting_gets_the_latest_state() {
        let (_d, sock, p) = setup(16, Duration::from_secs(5));
        drop(subscribe(&sock, "files-changed").unwrap());
        p.set_state(&99u32).unwrap();
        let sub = subscribe(&sock, "files-changed").unwrap();
        assert_eq!(sub.state.body_as::<u32>().unwrap(), 99);
    }

    #[test]
    fn dead_subscribers_are_removed() {
        let (_d, sock, p) = setup(16, Duration::from_secs(5));
        let sub = subscribe(&sock, "files-changed").unwrap();
        wait_for(|| p.subscribers() == 1);
        drop(sub);
        wait_for(|| {
            let _ = p.publish(&0u32);
            p.subscribers() == 0
        });
    }
}
