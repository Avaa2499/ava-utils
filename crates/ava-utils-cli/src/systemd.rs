use std::process::Command;

use ava_utils_config::error::{Error, Result};

pub trait Runner {
    fn run(&self, program: &str, args: &[String]) -> Result<String>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<String> {
        let out = Command::new(program).args(args).output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(Error::Usage(format!(
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

fn user(args: &[&str]) -> Vec<String> {
    std::iter::once("--user")
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

pub fn daemon_reload(r: &impl Runner) -> Result<()> {
    r.run("systemctl", &user(&["daemon-reload"])).map(|_| ())
}

pub fn enable_now(r: &impl Runner, module: &str) -> Result<()> {
    let unit = format!("ava-utils@{module}.socket");
    r.run("systemctl", &user(&["enable", "--now", &unit]))
        .map(|_| ())
}

pub fn disable_now(r: &impl Runner, module: &str) -> Result<()> {
    let socket = format!("ava-utils@{module}.socket");
    let service = format!("ava-utils@{module}.service");
    r.run("systemctl", &user(&["disable", "--now", &socket]))?;
    r.run("systemctl", &user(&["stop", &service])).map(|_| ())
}

pub fn restart_service(r: &impl Runner, module: &str) -> Result<()> {
    let service = format!("ava-utils@{module}.service");
    r.run("systemctl", &user(&["restart", &service]))
        .map(|_| ())
}

pub fn main_pid(r: &impl Runner, module: &str) -> Result<u32> {
    let service = format!("ava-utils@{module}.service");
    let out = r.run(
        "systemctl",
        &user(&["show", "-p", "MainPID", "--value", &service]),
    )?;
    out.parse()
        .map_err(|_| Error::Usage(format!("unexpected MainPID: {out:?}")))
}

#[derive(Debug, PartialEq, Eq)]
pub struct ModuleState {
    pub enabled: bool,
    pub socket_active: bool,
    pub service: String,
}

fn property(out: &str, key: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn module_state(r: &impl Runner, module: &str) -> Result<ModuleState> {
    let socket = r.run(
        "systemctl",
        &user(&[
            "show",
            "-p",
            "UnitFileState",
            "-p",
            "ActiveState",
            &format!("ava-utils@{module}.socket"),
        ]),
    )?;
    let service = r.run(
        "systemctl",
        &user(&[
            "show",
            "-p",
            "ActiveState",
            &format!("ava-utils@{module}.service"),
        ]),
    )?;
    Ok(ModuleState {
        enabled: property(&socket, "UnitFileState") == "enabled",
        socket_active: property(&socket, "ActiveState") == "active",
        service: property(&service, "ActiveState"),
    })
}

pub fn kill9(r: &impl Runner, pid: u32) -> Result<()> {
    r.run("kill", &["-9".to_string(), pid.to_string()])
        .map(|_| ())
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Recorder {
        pub calls: RefCell<Vec<String>>,
        pub pids: RefCell<Vec<String>>,
        pub shows: RefCell<Vec<String>>,
    }

    impl Runner for Recorder {
        fn run(&self, program: &str, args: &[String]) -> Result<String> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            if args.iter().any(|a| a == "MainPID") {
                let mut p = self.pids.borrow_mut();
                return Ok(if p.is_empty() {
                    "0".into()
                } else {
                    p.remove(0)
                });
            }
            if args.iter().any(|a| a == "show") {
                let mut p = self.shows.borrow_mut();
                if !p.is_empty() {
                    return Ok(p.remove(0));
                }
            }
            Ok(String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Recorder;
    use super::*;

    #[test]
    fn enable_targets_the_socket_unit() {
        let r = Recorder::default();
        enable_now(&r, "file-indexer").unwrap();
        assert_eq!(
            r.calls.borrow()[0],
            "systemctl --user enable --now ava-utils@file-indexer.socket"
        );
    }

    #[test]
    fn disable_stops_both_units() {
        let r = Recorder::default();
        disable_now(&r, "x").unwrap();
        let calls = r.calls.borrow();
        assert!(calls[0].contains("disable --now ava-utils@x.socket"));
        assert!(calls[1].contains("stop ava-utils@x.service"));
    }

    #[test]
    fn main_pid_is_parsed() {
        let r = Recorder::default();
        r.pids.borrow_mut().push("4242".into());
        assert_eq!(main_pid(&r, "x").unwrap(), 4242);
    }

    #[test]
    fn module_state_reads_the_socket_and_service_units() {
        let r = Recorder::default();
        r.shows.borrow_mut().extend([
            "ActiveState=active\nUnitFileState=enabled".into(),
            "ActiveState=inactive".into(),
        ]);
        assert_eq!(
            module_state(&r, "x").unwrap(),
            ModuleState {
                enabled: true,
                socket_active: true,
                service: "inactive".into()
            }
        );
        assert!(r.calls.borrow()[0].contains("ava-utils@x.socket"));
        assert!(r.calls.borrow()[1].contains("ava-utils@x.service"));
    }

    #[test]
    fn a_unit_that_systemd_does_not_know_reads_as_disabled_and_stopped() {
        let r = Recorder::default();
        let s = module_state(&r, "x").unwrap();
        assert!(!s.enabled && !s.socket_active && s.service.is_empty());
    }

    #[test]
    fn a_failing_command_is_an_error() {
        assert!(RealRunner.run("false", &[]).is_err());
        assert_eq!(RealRunner.run("echo", &["hi".into()]).unwrap(), "hi");
    }
}
