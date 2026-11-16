use std::path::{Path, PathBuf};

use ava_utils_config::error::{Error, Result};

use crate::install::Source;
use crate::systemd::Runner;

pub fn find_root(start: &Path) -> Result<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if std::fs::read_to_string(&manifest).is_ok_and(|t| t.contains("[workspace]")) {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }
    Err(Error::Usage(
        "not inside the ava-utils Cargo workspace".into(),
    ))
}

pub fn workspace_modules(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("manifest.toml").is_file())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("ava-utils-")
                .map(str::to_string)
        })
        .collect();
    found.sort();
    found
}

pub fn source_for(root: &Path, module: &str) -> Source {
    let package = format!("ava-utils-{module}");
    Source {
        module: module.to_string(),
        binary: root.join("target/release").join(&package),
        manifest: root.join("crates").join(&package).join("manifest.toml"),
    }
}

pub fn build_and_test(sys: &impl Runner, root: &Path, module: &str) -> Result<()> {
    let package = format!("ava-utils-{module}");
    let manifest_path = root.join("Cargo.toml").to_string_lossy().into_owned();
    for sub in [
        &["test", "-p", package.as_str()][..],
        &["build", "--release", "-p", package.as_str()][..],
    ] {
        let mut args: Vec<String> = sub.iter().map(|s| s.to_string()).collect();
        args.extend(["--manifest-path".to_string(), manifest_path.clone()]);
        sys.run("cargo", &args)
            .map_err(|e| Error::Usage(format!("cargo {} failed for {package}: {e}", sub[0])))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::fake::Recorder;

    #[test]
    fn finds_the_workspace_root_from_a_crate_folder() {
        let root = std::env::temp_dir().join(format!("au-ws-{}", std::process::id()));
        let deep = root.join("crates/x/src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []").unwrap();
        std::fs::write(root.join("crates/x/Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        assert_eq!(find_root(&deep).unwrap(), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_a_workspace_is_an_error() {
        assert!(find_root(Path::new("/")).is_err());
    }

    #[test]
    fn sources_follow_the_crate_layout() {
        let s = source_for(Path::new("/w"), "file-indexer");
        assert_eq!(
            s.binary,
            Path::new("/w/target/release/ava-utils-file-indexer")
        );
        assert_eq!(
            s.manifest,
            Path::new("/w/crates/ava-utils-file-indexer/manifest.toml")
        );
    }

    #[test]
    fn tests_run_before_the_release_build() {
        let r = Recorder::default();
        build_and_test(&r, Path::new("/w"), "file-indexer").unwrap();
        let calls = r.calls.borrow();
        assert!(calls[0].starts_with("cargo test -p ava-utils-file-indexer"));
        assert!(calls[1].starts_with("cargo build --release -p ava-utils-file-indexer"));
    }
}
