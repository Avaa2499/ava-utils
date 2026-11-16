use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ava_utils_config::client::retry;
use ava_utils_config::envelope::Envelope;
use ava_utils_config::error::codes;
use ava_utils_config::manifest::Manifest;
use ava_utils_config::messages::{
    CommandOutput, FILE_LOOKUP, FILES_CHANGED, FilesChanged, JOB_CANCEL, JOB_PROGRESS, JobCancel,
    JobProgress, RUN_COMMAND, RunCommand,
};
use ava_utils_config::paths::Dirs;
use ava_utils_config::server::{Handler, Listener, Outcome, serve};
use ava_utils_config::topic::subscribe;
use ava_utils_file_indexer::service::Service;

const MODULE: &str = "file-indexer";

struct Indexer(Arc<Service>);

impl Handler for Indexer {
    fn handle(&self, req: &Envelope) -> Outcome {
        let s = &self.0;
        let reply = match req.name.as_str() {
            RUN_COMMAND => req
                .body_as::<RunCommand>()
                .and_then(|c| s.run_command(&c.args))
                .and_then(|out: CommandOutput| Envelope::reply(req, &out)),
            JOB_PROGRESS => req
                .body_as::<JobProgress>()
                .and_then(|p| s.job_progress(&p))
                .and_then(|st| Envelope::reply(req, &st)),
            JOB_CANCEL => req
                .body_as::<JobCancel>()
                .and_then(|c| s.job_cancel(&c))
                .and_then(|()| Envelope::reply(req, &())),
            FILE_LOOKUP => req
                .body_as()
                .and_then(|q| s.file_lookup(&q))
                .and_then(|r| Envelope::reply(req, &r)),
            other => Envelope::error(req, codes::UNKNOWN_CALL, other),
        };
        let reply = reply
            .or_else(|e| Envelope::from_error(req, &e))
            .expect("replies always encode");
        Outcome::Reply(reply)
    }
}

fn follow_producer(service: Arc<Service>, dirs: Dirs) {
    let producer = Manifest::load(&dirs.manifests().join(format!("{MODULE}.toml")))
        .ok()
        .and_then(|m| m.listens.into_iter().find(|b| b.message == FILES_CHANGED))
        .map(|b| b.module);
    let Some(producer) = producer else {
        eprintln!("{MODULE}: manifest does not bind {FILES_CHANGED}, not following changes");
        return;
    };
    loop {
        let socket = match dirs.socket(&producer) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{MODULE}: {e}");
                return;
            }
        };
        let sub = retry(
            || subscribe(&socket, FILES_CHANGED),
            |w, e| {
                let level = if w.hard_error {
                    "HARD ERROR"
                } else {
                    "retrying"
                };
                eprintln!(
                    "{MODULE}: {producer} unavailable ({e}), {level}, next try in {:?}",
                    w.delay
                );
            },
            thread::sleep,
        );
        let Ok(mut sub) = sub else { continue };
        if let Ok(batch) = sub.state.body_as::<FilesChanged>() {
            let _ = service.files_changed(&batch);
        }
        while let Ok(ev) = sub.next_event(None) {
            match ev.body_as::<FilesChanged>() {
                Ok(batch) => {
                    if let Err(e) = service.files_changed(&batch) {
                        eprintln!("{MODULE}: could not apply a change batch: {e}");
                    }
                }
                Err(e) => eprintln!("{MODULE}: bad {FILES_CHANGED} event: {e}"),
            }
        }
        thread::sleep(Duration::from_secs(2));
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
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let service =
        Arc::new(Service::open(&state.join("index.redb"), workers).unwrap_or_else(|e| fail(&e)));
    let listener = Listener::from_systemd().unwrap_or_else(|e| fail(&e));
    eprintln!("{MODULE}: up, pid {}", std::process::id());
    {
        let (service, dirs) = (service.clone(), dirs.clone());
        thread::spawn(move || follow_producer(service, dirs));
    }
    serve(listener, Arc::new(Indexer(service)));
}
