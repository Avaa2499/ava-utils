use std::path::Path;

use ava_utils_config::error::Result;

pub const SOCKET_UNIT: &str = "ava-utils@.socket";
pub const SERVICE_UNIT: &str = "ava-utils@.service";

pub const SOCKET_TEMPLATE: &str = "[Unit]
Description=ava-utils %i socket

[Socket]
ListenStream=%t/ava-utils/%i.sock
TriggerLimitBurst=0

[Install]
WantedBy=sockets.target
";

pub const SERVICE_TEMPLATE: &str = "[Unit]
Description=ava-utils %i

[Service]
ExecStart=%h/.local/bin/ava-utils-%i
Restart=always
RestartSec=2s
RestartSteps=7
RestartMaxDelaySec=5min
";

pub fn ensure_templates(unit_dir: &Path) -> Result<bool> {
    std::fs::create_dir_all(unit_dir)?;
    let mut changed = false;
    for (name, text) in [
        (SOCKET_UNIT, SOCKET_TEMPLATE),
        (SERVICE_UNIT, SERVICE_TEMPLATE),
    ] {
        let path = unit_dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text) {
            std::fs::write(&path, text)?;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("au-units-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn writes_both_templates_once() {
        let d = dir("once");
        assert!(ensure_templates(&d).unwrap());
        assert!(!ensure_templates(&d).unwrap());
        assert_eq!(
            std::fs::read_to_string(d.join(SOCKET_UNIT)).unwrap(),
            SOCKET_TEMPLATE
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn repairs_a_template_that_was_edited() {
        let d = dir("repair");
        ensure_templates(&d).unwrap();
        std::fs::write(d.join(SERVICE_UNIT), "junk").unwrap();
        assert!(ensure_templates(&d).unwrap());
        assert_eq!(
            std::fs::read_to_string(d.join(SERVICE_UNIT)).unwrap(),
            SERVICE_TEMPLATE
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn templates_use_the_readme_restart_settings() {
        for s in [
            "Restart=always",
            "RestartSec=2s",
            "RestartSteps=7",
            "RestartMaxDelaySec=5min",
        ] {
            assert!(SERVICE_TEMPLATE.contains(s), "{s}");
        }
        assert!(SOCKET_TEMPLATE.contains("TriggerLimitBurst=0"));
    }
}
