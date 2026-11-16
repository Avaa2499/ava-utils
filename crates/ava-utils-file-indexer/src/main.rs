use std::sync::{Arc, Mutex};
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
use ava_utils_config::server::{Handler, Outcome, serve_module};
use ava_utils_config::topic::subscribe;
use ava_utils_file_indexer::follow::{Next, Start, Tracker, catch_up};
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
    let tracker = Arc::new(Mutex::new(Tracker::new(service.index.clone())));
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
        match sub.state.body_as::<FilesChanged>() {
            Ok(state) => {
                let (start, session) = {
                    let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
                    (t.begin(&state), t.session())
                };
                if let Err(e) = service.files_changed(&state) {
                    eprintln!("{MODULE}: could not apply the current state: {e}");
                }
                match start {
                    Ok(Start::Covered) => {
                        if let Err(e) = tracker
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .mark_applied()
                        {
                            eprintln!("{MODULE}: could not save the watcher position: {e}");
                        }
                    }
                    Ok(Start::NeedsScan(roots)) => {
                        eprintln!(
                            "{MODULE}: changes may have been missed, rescanning {} watched folders",
                            roots.len()
                        );
                        catch_up(service.clone(), tracker.clone(), session, roots);
                    }
                    Err(e) => eprintln!("{MODULE}: could not read the watcher position: {e}"),
                }
            }
            Err(e) => eprintln!("{MODULE}: bad {FILES_CHANGED} state: {e}"),
        }
        while let Ok(ev) = sub.next_event(None) {
            match ev.body_as::<FilesChanged>() {
                Ok(batch) => {
                    let next = tracker
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .event(&batch);
                    match next {
                        Next::Skip => {}
                        Next::Gap => {
                            eprintln!(
                                "{MODULE}: missed a {FILES_CHANGED} batch, reconnecting to catch up"
                            );
                            break;
                        }
                        Next::Apply => {
                            if let Err(e) = service.files_changed(&batch) {
                                eprintln!("{MODULE}: could not apply a change batch: {e}");
                            }
                            let (saved, fresh, session) = {
                                let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
                                (t.mark_applied(), t.new_roots(&batch), t.session())
                            };
                            if let Err(e) = saved {
                                eprintln!("{MODULE}: could not save the watcher position: {e}");
                            }
                            match fresh {
                                Ok(roots) if !roots.is_empty() => {
                                    eprintln!(
                                        "{MODULE}: {} new watched folders, scanning what is already in them",
                                        roots.len()
                                    );
                                    catch_up(service.clone(), tracker.clone(), session, roots);
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("{MODULE}: could not check the watched folders: {e}")
                                }
                            }
                        }
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
    {
        let (service, dirs) = (service.clone(), dirs.clone());
        thread::spawn(move || follow_producer(service, dirs));
    }
    serve_module(MODULE, Arc::new(Indexer(service))).unwrap_or_else(|e| fail(&e));
}
