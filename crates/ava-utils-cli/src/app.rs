use std::path::Path;

use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::load_all;

use crate::complete::{candidates, script};
use crate::help::help;
use crate::install::{Layout, Probe, SocketHealth, disable, enable, install, list, remove, status};
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
        Some("enable") => {
            let module = module_arg(&args, "enable")?;
            enable(layout, sys, &module)?;
            Ok(format!("enabled {module}"))
        }
        Some("disable") => {
            let module = module_arg(&args, "disable")?;
            disable(layout, sys, &module)?;
            Ok(format!(
                "disabled {module}: stopped, and it stays off until `au enable {module}`"
            ))
        }
        Some("status") => {
            let mut names: Vec<String> = load_all(&layout.dirs.manifests())?
                .into_iter()
                .map(|m| m.module)
                .collect();
            names.sort();
            Ok(status(sys, &SocketHealth(layout.dirs.clone()), &names))
        }
        Some("list") => {
            let mut names: Vec<String> = load_all(&layout.dirs.manifests())?
                .into_iter()
                .map(|m| m.module)
                .collect();
            names.sort();
            Ok(list(sys, &names))
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

    fn put_manifest(l: &Layout, module: &str) {
        std::fs::write(
            l.dirs.manifests().join(format!("{module}.toml")),
            format!("module = \"{module}\"\n"),
        )
        .unwrap();
    }

    fn go_with(l: &Layout, sys: &Recorder, args: &[&str]) -> Result<String> {
        dispatch(
            args.iter().map(|s| s.to_string()).collect(),
            l,
            sys,
            &NoProbe,
            Path::new("/"),
        )
    }

    #[test]
    fn list_with_nothing_installed_says_so() {
        let (root, l) = layout("list-none");
        assert_eq!(go(&l, &["list"]).unwrap(), "no modules installed");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn list_prints_one_sorted_line_per_module_with_enabled_and_running_state() {
        let (root, l) = layout("list");
        for m in ["zeta", "alpha", "mid"] {
            put_manifest(&l, m);
        }
        let sys = Recorder::default();
        sys.shows.borrow_mut().extend([
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=active".to_string(),
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=inactive".to_string(),
            "UnitFileState=disabled\nActiveState=inactive".to_string(),
            "ActiveState=inactive".to_string(),
        ]);
        let out = go_with(&l, &sys, &["list"]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("alpha")
                && lines[0].contains("enabled")
                && lines[0].contains("running"),
            "{out}"
        );
        assert!(
            lines[1].starts_with("mid") && lines[1].contains("idle"),
            "{out}"
        );
        assert!(
            lines[2].starts_with("zeta")
                && lines[2].contains("disabled")
                && lines[2].contains("stopped"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn list_marks_a_failed_service() {
        let (root, l) = layout("list-failed");
        put_manifest(&l, "bad");
        let sys = Recorder::default();
        sys.shows.borrow_mut().extend([
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=failed".to_string(),
        ]);
        assert!(go_with(&l, &sys, &["list"]).unwrap().contains("failed"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disable_stops_and_disables_an_installed_module() {
        let (root, l) = layout("disable");
        put_manifest(&l, "x");
        let sys = Recorder::default();
        let out = go_with(&l, &sys, &["disable", "x"]).unwrap();
        assert!(out.contains("au enable x"), "{out}");
        let calls = sys.calls.borrow();
        assert!(calls[0].contains("disable --now ava-utils@x.socket"));
        assert!(calls[1].contains("stop ava-utils@x.service"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enable_starts_the_socket_of_an_installed_module() {
        let (root, l) = layout("enable");
        put_manifest(&l, "x");
        let sys = Recorder::default();
        assert_eq!(go_with(&l, &sys, &["enable", "x"]).unwrap(), "enabled x");
        assert_eq!(
            sys.calls.borrow()[0],
            "systemctl --user enable --now ava-utils@x.socket"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enable_and_disable_refuse_a_module_that_is_not_installed_and_touch_nothing() {
        let (root, l) = layout("enable-missing");
        let sys = Recorder::default();
        for verb in ["enable", "disable"] {
            let e = go_with(&l, &sys, &[verb, "ghost"]).unwrap_err().to_string();
            assert!(e.contains("not installed"), "{e}");
        }
        assert!(sys.calls.borrow().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enable_and_disable_need_a_module_name_and_reject_path_tricks() {
        let (root, l) = layout("enable-usage");
        let sys = Recorder::default();
        for verb in ["enable", "disable"] {
            assert!(
                go_with(&l, &sys, &[verb])
                    .unwrap_err()
                    .to_string()
                    .contains("usage")
            );
            assert!(go_with(&l, &sys, &[verb, "../x"]).is_err());
        }
        assert!(sys.calls.borrow().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    use crate::install::{
        Health, HealthProbe, parse_health, short_uptime, status as render_status,
    };

    struct Fixed(Option<Health>);
    impl HealthProbe for Fixed {
        fn health(&self, _: &str) -> Option<Health> {
            self.0.as_ref().map(|h| Health {
                pid: h.pid.clone(),
                uptime: h.uptime.clone(),
                connections: h.connections.clone(),
                watching: h.watching.clone(),
                refused: h.refused.clone(),
            })
        }
    }

    fn some_health() -> Health {
        parse_health("pid=4242 uptime=3700 connections=2 watching=1 refused=3").unwrap()
    }

    #[test]
    fn health_text_is_parsed_and_garbage_is_not() {
        let h = some_health();
        assert_eq!(
            (h.pid.as_str(), h.uptime.as_str(), h.connections.as_str()),
            ("4242", "1h1m", "2")
        );
        assert_eq!((h.watching.as_str(), h.refused.as_str()), ("1", "3"));
        for bad in [
            "",
            "hello",
            "pid=1",
            "pid=1 uptime=x connections=1 watching=0 refused=0",
        ] {
            assert!(parse_health(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn uptimes_read_naturally() {
        assert_eq!(short_uptime(5), "5s");
        assert_eq!(short_uptime(125), "2m");
        assert_eq!(short_uptime(7260), "2h1m");
        assert_eq!(short_uptime(90_000), "1d1h");
    }

    #[test]
    fn status_with_nothing_installed_says_so() {
        assert_eq!(
            render_status(&Recorder::default(), &Fixed(None), &[]),
            "no modules installed"
        );
    }

    #[test]
    fn status_has_a_header_and_real_numbers_for_a_running_module() {
        let sys = Recorder::default();
        sys.shows.borrow_mut().extend([
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=active".to_string(),
        ]);
        let out = render_status(&sys, &Fixed(Some(some_health())), &["alpha".to_string()]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("module") && lines[0].contains("refused"));
        for want in ["alpha", "enabled", "running", "4242", "1h1m", "3"] {
            assert!(lines[1].contains(want), "{want}: {}", lines[1]);
        }
    }

    #[test]
    fn status_never_asks_a_module_that_is_not_running() {
        struct Panics;
        impl HealthProbe for Panics {
            fn health(&self, m: &str) -> Option<Health> {
                panic!("asked {m}");
            }
        }
        let sys = Recorder::default();
        sys.shows.borrow_mut().extend([
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=inactive".to_string(),
            "UnitFileState=disabled\nActiveState=inactive".to_string(),
            "ActiveState=inactive".to_string(),
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=failed".to_string(),
        ]);
        let names = ["idle", "off", "bad"].map(String::from);
        let out = render_status(&sys, &Panics, &names);
        assert!(
            out.contains("idle (starts on first call)")
                && out.contains("disabled")
                && out.contains("failed")
        );
        assert!(out.matches(" -").count() >= 15);
    }

    #[test]
    fn a_running_module_that_does_not_answer_shows_dashes() {
        let sys = Recorder::default();
        sys.shows.borrow_mut().extend([
            "UnitFileState=enabled\nActiveState=active".to_string(),
            "ActiveState=active".to_string(),
        ]);
        let out = render_status(&sys, &Fixed(None), &["alpha".to_string()]);
        assert!(out.lines().nth(1).unwrap().contains("running"));
        assert!(out.lines().nth(1).unwrap().matches(" -").count() >= 5);
    }

    #[test]
    fn status_is_a_builtin_and_lists_installed_modules_sorted() {
        let (root, l) = layout("status-cmd");
        put_manifest(&l, "zeta");
        put_manifest(&l, "alpha");
        let out = go(&l, &["status"]).unwrap();
        let rows: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("alpha") && rows[1].starts_with("zeta"));
        let _ = std::fs::remove_dir_all(root);
    }
}
