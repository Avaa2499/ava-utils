use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use ava_utils_config::client::{call, retry};
use ava_utils_config::envelope::Envelope;
use ava_utils_config::error::Error;
use ava_utils_config::messages::{CommandOutput, RUN_COMMAND, RunCommand};
use ava_utils_config::server::{Handler, Listener, Outcome, serve};

struct Toy;

impl Handler for Toy {
    fn handle(&self, req: &Envelope) -> Outcome {
        let reply = CommandOutput {
            text: format!("pid={}", std::process::id()),
            job: None,
        };
        Outcome::Reply(Envelope::reply(req, &reply).unwrap())
    }
}

#[test]
fn child_server() {
    let Some(sock) = std::env::var_os("AU_TOY_SOCK") else {
        return;
    };
    let listener = Listener::bind(Path::new(&sock)).unwrap();
    serve(listener, Arc::new(Toy));
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server(sock: &Path) -> KillOnDrop {
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["child_server", "--exact", "--nocapture", "--test-threads=1"])
        .env("AU_TOY_SOCK", sock)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    KillOnDrop(child)
}

fn ask(sock: &Path) -> Result<String, Error> {
    let args = RunCommand {
        args: vec!["ping".into()],
    };
    call(sock, RUN_COMMAND, &args)?
        .body_as::<CommandOutput>()
        .map(|o| o.text)
}

fn ask_until_up(sock: &Path) -> String {
    retry(
        || ask(sock),
        |_, _| {},
        |_| std::thread::sleep(Duration::from_millis(50)),
    )
    .unwrap()
}

fn sock_path() -> PathBuf {
    std::env::temp_dir().join(format!("au-two-proc-{}.sock", std::process::id()))
}

#[test]
fn a_killed_module_comes_back_and_callers_recover() {
    if std::env::var_os("AU_TOY_SOCK").is_some() {
        return;
    }
    let sock = sock_path();
    let _ = std::fs::remove_file(&sock);

    let first = spawn_server(&sock);
    let before = ask_until_up(&sock);
    assert!(before.starts_with("pid="));

    let mut first = first;
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(matches!(ask(&sock), Err(Error::Unavailable)));

    let _second = spawn_server(&sock);
    let after = ask_until_up(&sock);
    assert!(after.starts_with("pid="));
    assert_ne!(before, after);

    let _ = std::fs::remove_file(&sock);
}
