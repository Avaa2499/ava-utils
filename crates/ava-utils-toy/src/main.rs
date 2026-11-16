use std::sync::Arc;

use ava_utils_config::envelope::Envelope;
use ava_utils_config::messages::{CommandOutput, RUN_COMMAND, RunCommand};
use ava_utils_config::server::{Handler, Outcome, serve_module};

struct Toy;

impl Handler for Toy {
    fn handle(&self, req: &Envelope) -> Outcome {
        let reply = if req.name == RUN_COMMAND {
            req.body_as::<RunCommand>()
                .and_then(|c| {
                    if c.args.first().map(String::as_str) == Some("crash") {
                        std::process::abort();
                    }
                    let text = ava_utils_toy::run_command(&c.args, std::process::id());
                    Envelope::reply(req, &CommandOutput { text, job: None })
                })
                .or_else(|e| Envelope::from_error(req, &e))
        } else {
            Envelope::error(req, ava_utils_config::error::codes::UNKNOWN_CALL, &req.name)
        };
        Outcome::Reply(reply.expect("replies always encode"))
    }
}

fn main() {
    if let Err(e) = serve_module("toy", Arc::new(Toy)) {
        eprintln!("toy: {e}");
        std::process::exit(1);
    }
}
