use std::path::Path;

use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::load_all;

use crate::complete::{candidates, script};
use crate::help::help;
use crate::install::{Layout, Probe, install, remove};
use crate::loops;
use crate::run::run_module;
use crate::systemd::Runner;
use crate::workspace::{build_and_test, find_root, source_for, workspace_modules};

fn module_arg(args: &[String], verb: &str) -> Result<String> {
    args.get(1)
        .cloned()
        .ok_or_else(|| Error::Usage(format!("usage: au {verb} <module>")))
}

pub fn dispatch(
    mut args: Vec<String>,
    layout: &Layout,
    sys: &impl Runner,
    probe: &impl Probe,
    cwd: &Path,
) -> Result<String> {
    let raw = args.first().is_some_and(|a| a == "--raw");
    if raw {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            let manifests = load_all(&layout.dirs.manifests())?;
            Ok(help(&manifests, args.get(1).map(String::as_str)))
        }
        Some("install") => {
            let module = module_arg(&args, "install")?;
            let root = find_root(cwd)?;
            build_and_test(sys, &root, &module)?;
            install(layout, sys, probe, &source_for(&root, &module))?;
            Ok(format!("installed {module}"))
        }
        Some("remove") => {
            let module = module_arg(&args, "remove")?;
            remove(layout, sys, &module)?;
            Ok(format!("removed {module}"))
        }
        Some("__complete") => {
            let manifests = load_all(&layout.dirs.manifests()).unwrap_or_default();
            let workspace = find_root(cwd)
                .map(|r| workspace_modules(&r))
                .unwrap_or_default();
            Ok(candidates(&args[1..], &manifests, &workspace).join("\n"))
        }
        Some("completions") => script(args.get(1).map(String::as_str).unwrap_or("bash")),
        Some("check") => {
            loops::check(&load_all(&layout.dirs.manifests())?)?;
            Ok("no loops".to_string())
        }
        Some(module) => run_module(&layout.dirs, module, args[1..].to_vec(), raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::fake::Recorder;
    use ava_utils_config::paths::Dirs;
    use std::path::PathBuf;
    use std::time::Duration;

    struct NoProbe;
    impl Probe for NoProbe {
        fn ping(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn layout(tag: &str) -> (PathBuf, Layout) {
        let root = std::env::temp_dir().join(format!("au-app-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/ava-utils")).unwrap();
        let l = Layout {
            dirs: Dirs::new(
                root.join("run"),
                root.join("s"),
                root.join("c"),
                root.join("config"),
            ),
            unit_dir: root.join("units"),
            bin_dir: root.join("bin"),
            recovery_limit: Duration::from_millis(100),
        };
        (root, l)
    }

    fn go(l: &Layout, args: &[&str]) -> Result<String> {
        dispatch(
            args.iter().map(|s| s.to_string()).collect(),
            l,
            &Recorder::default(),
            &NoProbe,
            Path::new("/"),
        )
    }

    #[test]
    fn no_arguments_shows_help() {
        let (root, l) = layout("help");
        assert!(go(&l, &[]).unwrap().contains("au install <module>"));
        assert!(go(&l, &["help"]).unwrap().contains("au check"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn check_reports_loops_from_installed_manifests() {
        let (root, l) = layout("check");
        assert_eq!(go(&l, &["check"]).unwrap(), "no loops");
        let dir = l.dirs.manifests();
        std::fs::write(
            dir.join("a.toml"),
            "module = \"a\"\n[[calls]]\nmessage = \"x\"\nmodule = \"b\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.toml"),
            "module = \"b\"\n[[calls]]\nmessage = \"y\"\nmodule = \"a\"\n",
        )
        .unwrap();
        assert!(go(&l, &["check"]).unwrap_err().to_string().contains("loop"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_and_remove_need_a_module_name() {
        let (root, l) = layout("usage");
        assert!(
            go(&l, &["install"])
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
        assert!(
            go(&l, &["remove"])
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_outside_the_workspace_is_refused() {
        let (root, l) = layout("nows");
        let e = go(&l, &["install", "file-indexer"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("workspace"), "{e}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn anything_else_is_a_module_command() {
        let (root, l) = layout("module");
        let e = go(&l, &["file-indexer", "scan"]).unwrap_err().to_string();
        assert!(e.contains("not installed"), "{e}");
        let e = go(&l, &["--raw", "file-indexer"]).unwrap_err().to_string();
        assert!(e.contains("not installed"), "{e}");
        let _ = std::fs::remove_dir_all(root);
    }
}
