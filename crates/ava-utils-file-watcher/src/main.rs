use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use ava_utils_config::envelope::Envelope;
use ava_utils_config::messages::{
    CommandOutput, FILES_CHANGED, RUN_COMMAND, RunCommand, WATCH_SET_ROOTS, WatchSetRoots,
};
use ava_utils_config::paths::Dirs;
use ava_utils_config::server::{Handler, Listener, Outcome, serve};
use ava_utils_file_watcher::batch::pump;
use ava_utils_file_watcher::service::{Hub, Service};

const MODULE: &str = "file-watcher";

struct Watcher {
    service: Service,
    hub: Arc<Hub>,
}

impl Handler for Watcher {
    fn handle(&self, req: &Envelope) -> Outcome {
        if req.name == FILES_CHANGED {
            return Outcome::Subscribe(self.hub.publisher.clone());
        }
        let reply = match req.name.as_str() {
            RUN_COMMAND => req
                .body_as::<RunCommand>()
                .and_then(|c| self.service.run_command(&c.args))
                .and_then(|text| Envelope::reply(req, &CommandOutput { text, job: None })),
            WATCH_SET_ROOTS => req
                .body_as::<WatchSetRoots>()
                .and_then(|r| self.service.set_roots(&r.roots))
                .and_then(|_| Envelope::reply(req, &())),
            other => Envelope::error(req, "unknown-call", other),
        };
        Outcome::Reply(
            reply
                .or_else(|e| Envelope::error(req, "failed", &e.to_string()))
                .expect("replies always encode"),
        )
    }
}

fn main() {
    let fail = |e: &dyn std::fmt::Display| -> ! {
        eprintln!("{MODULE}: {e}");
        std::process::exit(1)
    };
    let dirs = Dirs::from_env().unwrap_or_else(|e| fail(&e));
    let state = dirs.state(MODULE).unwrap_or_else(|e| fail(&e));
    std::fs::create_dir_all(&state).unwrap_or_else(|e| fail(&e));
    let hub = Arc::new(Hub::new(FILES_CHANGED).unwrap_or_else(|e| fail(&e)));
    let (tx, rx) = channel();
    let (service, warnings) = Service::start(&state, tx).unwrap_or_else(|e| fail(&e));
    for w in warnings {
        eprintln!("{MODULE}: {w}");
    }
    {
        let hub = hub.clone();
        thread::spawn(move || {
            pump(
                &rx,
                Duration::from_millis(300),
                Duration::from_secs(2),
                |batch| {
                    if let Err(e) = hub.emit(&batch) {
                        eprintln!("{MODULE}: could not publish a batch: {e}");
                    }
                },
            )
        });
    }
    let listener = Listener::from_systemd().unwrap_or_else(|e| fail(&e));
    eprintln!("{MODULE}: up, pid {}", std::process::id());
    serve(listener, Arc::new(Watcher { service, hub }));
}
