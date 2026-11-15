use ava_utils_config::client::call;
use ava_utils_config::error::{Error, Result};
use ava_utils_config::messages::{CommandOutput, RUN_COMMAND, RunCommand};
use ava_utils_config::paths::Dirs;

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .map(|c| {
            c.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn run_module(dirs: &Dirs, module: &str, args: Vec<String>, raw: bool) -> Result<String> {
    let manifest = dirs.manifests().join(format!("{module}.toml"));
    if !manifest.exists() {
        return Err(Error::Usage(format!(
            "{module} is not installed (try `au help`)"
        )));
    }
    let reply = match call(&dirs.socket(module)?, RUN_COMMAND, &RunCommand { args }) {
        Err(Error::Unavailable) => {
            return Err(Error::Usage(format!("{module} is not answering")));
        }
        Err(Error::Timeout) => {
            return Err(Error::Usage(format!("{module} did not reply within 5s")));
        }
        other => other?,
    };
    if raw {
        return Ok(hex(&reply.encode()?));
    }
    let out: CommandOutput = reply.body_as()?;
    Ok(match out.job {
        Some(job) => format!("{}\njob {job}", out.text),
        None => out.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ava_utils_config::envelope::Envelope;
    use ava_utils_config::frame::{read_frame, write_frame};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    fn sandbox(tag: &str) -> (PathBuf, Dirs) {
        let root = std::env::temp_dir().join(format!("au-run-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("run/ava-utils")).unwrap();
        std::fs::create_dir_all(root.join("config/ava-utils")).unwrap();
        let dirs = Dirs::new(
            root.join("run"),
            root.join("s"),
            root.join("c"),
            root.join("config"),
        );
        (root, dirs)
    }

    fn install_fake(
        dirs: &Dirs,
        root: &std::path::Path,
        reply: CommandOutput,
    ) -> thread::JoinHandle<Vec<String>> {
        std::fs::write(root.join("config/ava-utils/m.toml"), "module = \"m\"").unwrap();
        let listener = UnixListener::bind(dirs.socket("m").unwrap()).unwrap();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let req = read_frame(&mut s).unwrap();
            let got: RunCommand = req.body_as().unwrap();
            write_frame(&mut s, &Envelope::reply(&req, &reply).unwrap()).unwrap();
            got.args
        })
    }

    #[test]
    fn sends_the_raw_arguments_and_prints_the_reply_text() {
        let (root, dirs) = sandbox("text");
        let h = install_fake(
            &dirs,
            &root,
            CommandOutput {
                text: "412 files".into(),
                job: None,
            },
        );
        let out = run_module(&dirs, "m", vec!["status".into(), "--x".into()], false).unwrap();
        assert_eq!(out, "412 files");
        assert_eq!(h.join().unwrap(), ["status", "--x"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_started_job_prints_its_id() {
        let (root, dirs) = sandbox("job");
        let h = install_fake(
            &dirs,
            &root,
            CommandOutput {
                text: "started".into(),
                job: Some("7f3a9c".into()),
            },
        );
        assert_eq!(
            run_module(&dirs, "m", vec![], false).unwrap(),
            "started\njob 7f3a9c"
        );
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn raw_shows_the_bytes_instead() {
        let (root, dirs) = sandbox("raw");
        let h = install_fake(
            &dirs,
            &root,
            CommandOutput {
                text: "hi".into(),
                job: None,
            },
        );
        let out = run_module(&dirs, "m", vec![], true).unwrap();
        assert!(
            out.split_whitespace()
                .all(|b| b.len() == 2 && u8::from_str_radix(b, 16).is_ok())
        );
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_uninstalled_module_is_a_clear_error() {
        let (root, dirs) = sandbox("none");
        let e = run_module(&dirs, "m", vec![], false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not installed"), "{e}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_installed_but_dead_module_says_so() {
        let (root, dirs) = sandbox("dead");
        std::fs::write(root.join("config/ava-utils/m.toml"), "module = \"m\"").unwrap();
        let e = run_module(&dirs, "m", vec![], false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not answering"), "{e}");
        let _ = std::fs::remove_dir_all(root);
    }
}
