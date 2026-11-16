use std::sync::Arc;

use ava_utils_config::envelope::Envelope;
use ava_utils_config::messages::{CommandOutput, RUN_COMMAND, RunCommand};
use ava_utils_config::server::{Handler, Listener, Outcome, serve};

struct Toy;

impl Handler for Toy {
    fn handle(&self, req: &Envelope) -> Outcome {
        let reply = if req.name == RUN_COMMAND {
            req.body_as::<RunCommand>()
                .and_then(|c| {
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
    let listener = match Listener::from_systemd() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("toy: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("toy: up, pid {}", std::process::id());
    serve(listener, Arc::new(Toy));
}
