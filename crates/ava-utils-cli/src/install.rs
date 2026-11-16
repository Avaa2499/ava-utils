use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ava_utils_config::client::{self, call};
use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::{Manifest, load_all};
use ava_utils_config::messages::{CommandOutput, RUN_COMMAND, RunCommand};
use ava_utils_config::paths::{Dirs, check_module_name};
use ava_utils_config::server::HEALTH;

use crate::loops;
use crate::systemd::{self, Runner};
use crate::units;

pub const RESERVED: [&str; 9] = [
    "status",
    "install",
    "remove",
    "check",
    "help",
    "completions",
    "list",
    "enable",
    "disable",
];

pub const CRASH_RECOVERY_LIMIT: Duration = Duration::from_secs(15);

pub struct Layout {
    pub dirs: Dirs,
    pub unit_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub recovery_limit: Duration,
}

impl Layout {
    pub fn from_env() -> Result<Self> {
        let dirs = Dirs::from_env()?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Usage("HOME is not set".into()))?;
        let config = dirs
            .manifests()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"));
        Ok(Self {
            dirs,
            unit_dir: config.join("systemd/user"),
            bin_dir: home.join(".local/bin"),
            recovery_limit: CRASH_RECOVERY_LIMIT,
        })
    }

    fn binary(&self, module: &str) -> PathBuf {
        self.bin_dir.join(format!("ava-utils-{module}"))
    }

    fn manifest(&self, module: &str) -> PathBuf {
        self.dirs.manifests().join(format!("{module}.toml"))
    }
}

pub struct Source {
    pub module: String,
    pub binary: PathBuf,
    pub manifest: PathBuf,
}

pub trait Probe {
    fn ping(&self, module: &str) -> Result<()>;
}

pub struct SocketProbe(pub Dirs);

impl Probe for SocketProbe {
    fn ping(&self, module: &str) -> Result<()> {
        let sock = self.0.socket(module)?;
        call(&sock, RUN_COMMAND, &RunCommand { args: vec![] }).map(|_| ())
    }
}

struct Backup {
    binary: Option<PathBuf>,
    manifest: Option<PathBuf>,
}

fn stash(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let old = path.with_extension("old");
    std::fs::rename(path, &old)?;
    Ok(Some(old))
}

fn put_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = to.with_extension("new");
    std::fs::copy(from, &tmp)?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

fn unlink(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn install(layout: &Layout, sys: &impl Runner, probe: &impl Probe, src: &Source) -> Result<()> {
    check_module_name(&src.module)?;
    if RESERVED.contains(&src.module.as_str()) {
        return Err(Error::Usage(format!(
            "{} is a built-in au command, pick another module name",
            src.module
        )));
    }
    let manifest = Manifest::parse(&std::fs::read_to_string(&src.manifest)?)?;
    if manifest.module != src.module {
        return Err(Error::Usage(format!(
            "manifest declares module {} but you are installing {}",
            manifest.module, src.module
        )));
    }
    let mut all: Vec<Manifest> = load_all(&layout.dirs.manifests())?
        .into_iter()
        .filter(|m| m.module != src.module)
        .collect();
    let others: Vec<String> = all.iter().map(|m| m.module.clone()).collect();
    all.push(manifest);
    loops::check(&all)?;

    let bin = layout.binary(&src.module);
    let man = layout.manifest(&src.module);
    let backup = Backup {
        binary: stash(&bin)?,
        manifest: stash(&man)?,
    };
    let result = put_and_start(layout, sys, probe, src, &bin, &man, &others);
    match result {
        Ok(()) => {
            for old in [&backup.binary, &backup.manifest].into_iter().flatten() {
                unlink(old);
            }
            Ok(())
        }
        Err(e) => {
            let _ = systemd::disable_now(sys, &src.module);
            unlink(&bin);
            unlink(&man);
            if let Some(old) = &backup.binary {
                let _ = std::fs::rename(old, &bin);
            }
            if let Some(old) = &backup.manifest {
                let _ = std::fs::rename(old, &man);
            }
            let _ = systemd::daemon_reload(sys);
            Err(e)
        }
    }
}

fn put_and_start(
    layout: &Layout,
    sys: &impl Runner,
    probe: &impl Probe,
    src: &Source,
    bin: &Path,
    man: &Path,
    others: &[String],
) -> Result<()> {
    put_file(&src.binary, bin)?;
    put_file(&src.manifest, man)?;
    units::ensure_templates(&layout.unit_dir)?;
    systemd::daemon_reload(sys)?;
    systemd::enable_now(sys, &src.module)?;
    systemd::restart_service(sys, &src.module)?;
    crash_test(sys, probe, &src.module, others, layout.recovery_limit)
}

fn crash_test(
    sys: &impl Runner,
    probe: &impl Probe,
    module: &str,
    others: &[String],
    limit: Duration,
) -> Result<()> {
    probe.ping(module)?;
    let pid = systemd::main_pid(sys, module)?;
    if pid == 0 {
        return Err(Error::Usage(format!(
            "{module} answered but is not running"
        )));
    }
    let others_before = pids(sys, others)?;
    systemd::kill9(sys, pid)?;
    let started = Instant::now();
    loop {
        if probe.ping(module).is_ok() {
            let now = systemd::main_pid(sys, module)?;
            if now != 0 && now != pid {
                break;
            }
        }
        if started.elapsed() > limit {
            return Err(Error::Usage(format!(
                "{module} did not come back within {}ms after kill -9",
                limit.as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if pids(sys, others)? != others_before {
        return Err(Error::Usage(format!(
            "killing {module} changed another module"
        )));
    }
    Ok(())
}

fn pids(sys: &impl Runner, modules: &[String]) -> Result<Vec<u32>> {
    modules.iter().map(|m| systemd::main_pid(sys, m)).collect()
}

fn installed(layout: &Layout, module: &str) -> Result<()> {
    check_module_name(module)?;
    if layout.manifest(module).exists() {
        Ok(())
    } else {
        Err(Error::Usage(format!("{module} is not installed")))
    }
}

pub fn enable(layout: &Layout, sys: &impl Runner, module: &str) -> Result<()> {
    installed(layout, module)?;
    systemd::enable_now(sys, module)
}

pub fn disable(layout: &Layout, sys: &impl Runner, module: &str) -> Result<()> {
    installed(layout, module)?;
    systemd::disable_now(sys, module)
}

pub fn list(sys: &impl Runner, modules: &[String]) -> String {
    if modules.is_empty() {
        return "no modules installed".to_string();
    }
    let width = modules.iter().map(String::len).max().unwrap_or(0);
    modules
        .iter()
        .map(|m| {
            let (enabled, status) = match systemd::module_state(sys, m) {
                Ok(s) => (
                    if s.enabled { "enabled" } else { "disabled" },
                    state_text(&s),
                ),
                Err(_) => ("unknown", "systemd did not answer"),
            };
            format!("{m:<width$}  {enabled:<8}  {status}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct Health {
    pub pid: String,
    pub uptime: String,
    pub connections: String,
    pub watching: String,
    pub refused: String,
}

pub trait HealthProbe {
    fn health(&self, module: &str) -> Option<Health>;
}

pub struct SocketHealth(pub Dirs);

impl HealthProbe for SocketHealth {
    fn health(&self, module: &str) -> Option<Health> {
        let sock = self.0.socket(module).ok()?;
        let reply = client::call(
            &sock,
            RUN_COMMAND,
            &RunCommand {
                args: vec![HEALTH.to_string()],
            },
        )
        .ok()?;
        parse_health(&reply.body_as::<CommandOutput>().ok()?.text)
    }
}

pub fn parse_health(text: &str) -> Option<Health> {
    let get = |key: &str| {
        text.split_whitespace()
            .find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .map(str::to_string)
    };
    Some(Health {
        pid: get("pid")?,
        uptime: get("uptime")?.parse().ok().map(short_uptime)?,
        connections: get("connections")?,
        watching: get("watching")?,
        refused: get("refused")?,
    })
}

pub fn short_uptime(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{}h", secs / 86400, (secs % 86400) / 3600),
    }
}

pub fn status(sys: &impl Runner, probe: &impl HealthProbe, modules: &[String]) -> String {
    if modules.is_empty() {
        return "no modules installed".to_string();
    }
    let width = modules.iter().map(String::len).max().unwrap_or(0);
    let mut out = format!(
        "{:<width$}  {:<8}  {:<28}  {:>7}  {:>6}  {:>5}  {:>8}  {:>7}\n",
        "module", "enabled", "state", "pid", "up", "conns", "watching", "refused"
    );
    let rows: Vec<String> = modules
        .iter()
        .map(|m| {
            let Ok(s) = systemd::module_state(sys, m) else {
                return format!("{m:<width$}  unknown   systemd did not answer");
            };
            let state = state_text(&s);
            let enabled = if s.enabled { "enabled" } else { "disabled" };
            let h = if s.service == "active" {
                probe.health(m)
            } else {
                None
            };
            match h {
                Some(h) => format!(
                    "{m:<width$}  {enabled:<8}  {state:<28}  {:>7}  {:>6}  {:>5}  {:>8}  {:>7}",
                    h.pid, h.uptime, h.connections, h.watching, h.refused
                ),
                None => format!(
                    "{m:<width$}  {enabled:<8}  {state:<28}  {:>7}  {:>6}  {:>5}  {:>8}  {:>7}",
                    "-", "-", "-", "-", "-"
                ),
            }
        })
        .collect();
    out.push_str(&rows.join("\n"));
    out
}

fn state_text(s: &systemd::ModuleState) -> &'static str {
    match s.service.as_str() {
        "active" => "running",
        "activating" | "reloading" => "starting",
        "failed" => "failed",
        _ if s.socket_active => "idle (starts on first call)",
        _ => "stopped",
    }
}

pub fn remove(layout: &Layout, sys: &impl Runner, module: &str) -> Result<()> {
    check_module_name(module)?;
    if !layout.manifest(module).exists() && !layout.binary(module).exists() {
        return Err(Error::Usage(format!("{module} is not installed")));
    }
    let _ = systemd::disable_now(sys, module);
    unlink(&layout.binary(module));
    unlink(&layout.manifest(module));
    systemd::daemon_reload(sys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::fake::Recorder;
    use std::cell::Cell;

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("au-install-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("src")).unwrap();
            Self(root)
        }

        fn layout(&self) -> Layout {
            Layout {
                dirs: Dirs::new(
                    self.0.join("run"),
                    self.0.join("state"),
                    self.0.join("cache"),
                    self.0.join("config"),
                ),
                unit_dir: self.0.join("units"),
                bin_dir: self.0.join("bin"),
                recovery_limit: Duration::from_millis(300),
            }
        }

        fn source(&self, module: &str, manifest: &str) -> Source {
            let binary = self.0.join("src").join(format!("{module}.bin"));
            let man = self.0.join("src").join(format!("{module}.toml"));
            std::fs::write(&binary, b"binary").unwrap();
            std::fs::write(&man, manifest).unwrap();
            Source {
                module: module.into(),
                binary,
                manifest: man,
            }
        }

        fn existing(&self, module: &str, manifest: &str) {
            let dir = self.layout().dirs.manifests();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{module}.toml")), manifest).unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct AlwaysUp;
    impl Probe for AlwaysUp {
        fn ping(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    struct DeadAfterKill {
        pinged: Cell<u32>,
    }
    impl Probe for DeadAfterKill {
        fn ping(&self, _: &str) -> Result<()> {
            self.pinged.set(self.pinged.get() + 1);
            if self.pinged.get() == 1 {
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        }
    }

    fn manifest(module: &str, calls: &str) -> String {
        format!("module = \"{module}\"\nanswers = [\"run-command\"]\n{calls}")
    }

    #[test]
    fn installs_a_module_and_runs_the_crash_test() {
        let sb = Sandbox::new("ok");
        let l = sb.layout();
        let sys = Recorder::default();
        sys.pids
            .borrow_mut()
            .extend(["100".to_string(), "200".to_string()]);
        let src = sb.source("file-indexer", &manifest("file-indexer", ""));
        install(&l, &sys, &AlwaysUp, &src).unwrap();
        assert!(l.binary("file-indexer").exists());
        assert!(l.manifest("file-indexer").exists());
        assert!(l.unit_dir.join(units::SOCKET_UNIT).exists());
        let calls = sys.calls.borrow().join("\n");
        assert!(
            calls.contains("enable --now ava-utils@file-indexer.socket"),
            "{calls}"
        );
        assert!(calls.contains("kill -9 100"), "{calls}");
        let restart = calls.find("restart ava-utils@file-indexer.service");
        let kill = calls.find("kill -9 100");
        assert!(
            restart.is_some() && restart < kill,
            "the new binary must be restarted in (resetting the crash counter) before the crash test: {calls}"
        );
    }

    #[test]
    fn a_loop_is_refused_before_anything_is_touched() {
        let sb = Sandbox::new("loop");
        sb.existing(
            "b",
            &manifest("b", "[[calls]]\nmessage = \"x\"\nmodule = \"a\"\n"),
        );
        let src = sb.source(
            "a",
            &manifest("a", "[[calls]]\nmessage = \"y\"\nmodule = \"b\"\n"),
        );
        let sys = Recorder::default();
        let e = install(&sb.layout(), &sys, &AlwaysUp, &src).unwrap_err();
        assert!(e.to_string().contains("loop"), "{e}");
        assert!(sys.calls.borrow().is_empty());
        assert!(!sb.layout().binary("a").exists());
    }

    #[test]
    fn built_in_command_names_cannot_be_modules() {
        let sb = Sandbox::new("reserved");
        let src = sb.source("install", &manifest("install", ""));
        let e = install(&sb.layout(), &Recorder::default(), &AlwaysUp, &src).unwrap_err();
        assert!(e.to_string().contains("built-in"), "{e}");
    }

    #[test]
    fn a_manifest_for_the_wrong_module_is_refused() {
        let sb = Sandbox::new("name");
        let src = sb.source("a", &manifest("b", ""));
        let e = install(&sb.layout(), &Recorder::default(), &AlwaysUp, &src).unwrap_err();
        assert!(e.to_string().contains("declares module b"), "{e}");
    }

    #[test]
    fn a_module_that_does_not_come_back_is_rolled_back() {
        let sb = Sandbox::new("dead");
        let l = sb.layout();
        let src = sb.source("a", &manifest("a", ""));
        let sys = Recorder::default();
        sys.pids.borrow_mut().push("100".into());
        let probe = DeadAfterKill {
            pinged: Cell::new(0),
        };
        let started = Instant::now();
        let e = install_with_short_limit(&l, &sys, &probe, &src);
        assert!(e.to_string().contains("did not come back"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!l.binary("a").exists());
        assert!(!l.manifest("a").exists());
        assert!(
            sys.calls
                .borrow()
                .join("\n")
                .contains("disable --now ava-utils@a.socket")
        );
    }

    fn install_with_short_limit(
        l: &Layout,
        sys: &Recorder,
        probe: &DeadAfterKill,
        src: &Source,
    ) -> Error {
        install(l, sys, probe, src).unwrap_err()
    }

    #[test]
    fn a_reinstall_failure_restores_the_old_version() {
        let sb = Sandbox::new("restore");
        let l = sb.layout();
        std::fs::create_dir_all(&l.bin_dir).unwrap();
        std::fs::write(l.binary("a"), b"old binary").unwrap();
        sb.existing("a", &manifest("a", ""));
        let src = sb.source("a", &manifest("a", ""));
        let sys = Recorder::default();
        sys.pids.borrow_mut().push("0".into());
        assert!(install(&l, &sys, &AlwaysUp, &src).is_err());
        assert_eq!(std::fs::read(l.binary("a")).unwrap(), b"old binary");
        assert!(l.manifest("a").exists());
    }

    #[test]
    fn another_module_changing_during_the_crash_test_is_caught() {
        let sb = Sandbox::new("other");
        sb.existing("other", &manifest("other", ""));
        let src = sb.source("a", &manifest("a", ""));
        let sys = Recorder::default();
        sys.pids
            .borrow_mut()
            .extend(["100", "500", "101", "999"].map(String::from));
        let e = install(&sb.layout(), &sys, &AlwaysUp, &src).unwrap_err();
        assert!(e.to_string().contains("changed another module"), "{e}");
        assert!(!sb.layout().binary("a").exists());
    }

    #[test]
    fn remove_deletes_files_and_stops_units() {
        let sb = Sandbox::new("remove");
        let l = sb.layout();
        sb.existing("a", &manifest("a", ""));
        std::fs::create_dir_all(&l.bin_dir).unwrap();
        std::fs::write(l.binary("a"), b"x").unwrap();
        let sys = Recorder::default();
        remove(&l, &sys, "a").unwrap();
        assert!(!l.binary("a").exists() && !l.manifest("a").exists());
        assert!(
            sys.calls
                .borrow()
                .join("\n")
                .contains("disable --now ava-utils@a.socket")
        );
        assert!(remove(&l, &sys, "a").is_err());
    }
}
