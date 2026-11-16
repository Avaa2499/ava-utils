use std::path::{Path, PathBuf};

use ava_utils_config::error::{Error, Result};

pub struct Roots {
    file: PathBuf,
}

fn canonical(p: &str) -> Result<String> {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .map_err(|e| Error::Usage(format!("cannot watch {p}: {e}")))
}

impl Roots {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            file: state_dir.join("roots.txt"),
        }
    }

    pub fn load(&self) -> Result<Vec<String>> {
        match std::fs::read_to_string(&self.file) {
            Ok(text) => Ok(text
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, roots: &[String]) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.file.with_extension("tmp");
        std::fs::write(&tmp, roots.join("\n"))?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    pub fn canonicalize_all(roots: &[String]) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for r in roots {
            let c = canonical(r)?;
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = PathBuf::from("/var/tmp/au-fw-tests").join(format!("{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roots_survive_a_reload_and_a_missing_file_means_none() {
        let d = dir("roots");
        let r = Roots::new(&d);
        assert!(r.load().unwrap().is_empty());
        r.save(&["/a".into(), "/b".into()]).unwrap();
        assert_eq!(Roots::new(&d).load().unwrap(), ["/a", "/b"]);
        r.save(&[]).unwrap();
        assert!(r.load().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn roots_are_made_absolute_unique_and_must_exist() {
        let d = dir("canon");
        let s = d.to_string_lossy().into_owned();
        let got = Roots::canonicalize_all(&[s.clone(), format!("{s}/.")]).unwrap();
        assert_eq!(got, [s]);
        assert!(Roots::canonicalize_all(&["/definitely/not/here".into()]).is_err());
        let _ = std::fs::remove_dir_all(d);
    }
}
