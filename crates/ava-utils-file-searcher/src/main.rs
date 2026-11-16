use std::sync::Arc;

use ava_utils_config::client::call;
use ava_utils_config::envelope::Envelope;
use ava_utils_config::error::{Error, Result, codes};
use ava_utils_config::manifest::Manifest;
use ava_utils_config::messages::{
    CommandOutput, FILE_LOOKUP, FileLookup, FileLookupResult, RUN_COMMAND, RunCommand, SEARCH,
    Search,
};
use ava_utils_config::paths::Dirs;
use ava_utils_config::server::{Handler, Listener, Outcome, serve};
use ava_utils_file_searcher::service::{run_command, search};

const MODULE: &str = "file-searcher";

struct Searcher {
    dirs: Dirs,
}

impl Searcher {
    fn lookup(&self, q: &FileLookup) -> Result<FileLookupResult> {
        let manifest = Manifest::load(&self.dirs.manifests().join(format!("{MODULE}.toml")))?;
        let target = manifest
            .calls
            .iter()
            .find(|b| b.message == FILE_LOOKUP)
            .ok_or_else(|| Error::Protocol(format!("manifest does not bind {FILE_LOOKUP}")))?;
        call(&self.dirs.socket(&target.module)?, FILE_LOOKUP, q)
            .and_then(|r| r.body_as())
            .map_err(|e| match e {
                Error::Unavailable | Error::Timeout => Error::Remote {
                    code: codes::UNAVAILABLE.into(),
                    message: "the file index is not reachable".into(),
                },
                other => other,
            })
    }
}

impl Handler for Searcher {
    fn handle(&self, req: &Envelope) -> Outcome {
        let mut lookup = |q: &FileLookup| self.lookup(q);
        let reply = match req.name.as_str() {
            RUN_COMMAND => req
                .body_as::<RunCommand>()
                .and_then(|c| run_command(&mut lookup, &c.args))
                .and_then(|text| Envelope::reply(req, &CommandOutput { text, job: None })),
            SEARCH => req
                .body_as::<Search>()
                .and_then(|s| search(&mut lookup, &s))
                .and_then(|r| Envelope::reply(req, &r)),
            other => Envelope::error(req, codes::UNKNOWN_CALL, other),
        };
        let reply = reply
            .or_else(|e| Envelope::from_error(req, &e))
            .expect("replies always encode");
        Outcome::Reply(reply)
    }
}

fn main() {
    let fail = |e: &dyn std::fmt::Display| -> ! {
        eprintln!("{MODULE}: {e}");
        std::process::exit(1)
    };
    let dirs = Dirs::from_env().unwrap_or_else(|e| fail(&e));
    let listener = Listener::from_systemd().unwrap_or_else(|e| fail(&e));
    eprintln!("{MODULE}: up, pid {}", std::process::id());
    serve(listener, Arc::new(Searcher { dirs }));
}
