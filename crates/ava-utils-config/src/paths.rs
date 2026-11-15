use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Dirs {
    runtime: PathBuf,
    state_home: PathBuf,
    cache_home: PathBuf,
    config_home: PathBuf,
}

pub fn check_module_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Protocol(format!("bad module name: {name:?}")))
    }
}

impl Dirs {
    pub fn new(
        runtime: PathBuf,
        state_home: PathBuf,
        cache_home: PathBuf,
        config_home: PathBuf,
    ) -> Self {
        Self {
            runtime,
            state_home,
            cache_home,
            config_home,
        }
    }

    pub fn from_env() -> Result<Self> {
        let get = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = get("HOME");
        let under_home = |rel: &str| {
            home.clone()
                .map(|h| h.join(rel))
                .ok_or_else(|| Error::Protocol("HOME is not set".into()))
        };
        Ok(Self {
            runtime: get("XDG_RUNTIME_DIR")
                .ok_or_else(|| Error::Protocol("XDG_RUNTIME_DIR is not set".into()))?,
            state_home: match get("XDG_STATE_HOME") {
                Some(p) => p,
                None => under_home(".local/state")?,
            },
            cache_home: match get("XDG_CACHE_HOME") {
                Some(p) => p,
                None => under_home(".cache")?,
            },
            config_home: match get("XDG_CONFIG_HOME") {
                Some(p) => p,
                None => under_home(".config")?,
            },
        })
    }

    pub fn socket(&self, module: &str) -> Result<PathBuf> {
        check_module_name(module)?;
        Ok(self
            .runtime
            .join("ava-utils")
            .join(format!("{module}.sock")))
    }

    pub fn state(&self, module: &str) -> Result<PathBuf> {
        check_module_name(module)?;
        Ok(self.state_home.join("ava-utils").join(module))
    }

    pub fn cache(&self, module: &str) -> Result<PathBuf> {
        check_module_name(module)?;
        Ok(self.cache_home.join("ava-utils").join(module))
    }

    pub fn manifests(&self) -> PathBuf {
        self.config_home.join("ava-utils")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs::new("/run/u".into(), "/st".into(), "/ca".into(), "/cf".into())
    }

    #[test]
    fn paths_follow_the_readme_layout() {
        let d = dirs();
        assert_eq!(
            d.socket("file-indexer").unwrap(),
            PathBuf::from("/run/u/ava-utils/file-indexer.sock")
        );
        assert_eq!(
            d.state("file-indexer").unwrap(),
            PathBuf::from("/st/ava-utils/file-indexer")
        );
        assert_eq!(
            d.cache("file-indexer").unwrap(),
            PathBuf::from("/ca/ava-utils/file-indexer")
        );
        assert_eq!(d.manifests(), PathBuf::from("/cf/ava-utils"));
    }

    #[test]
    fn module_names_cannot_escape_their_folder() {
        let d = dirs();
        for bad in ["", "..", "../x", "a/b", "A", "x y", "-x", "x-"] {
            assert!(d.state(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
