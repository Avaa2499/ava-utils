use std::fmt;
use std::path::Path;

use ava_utils_config::error::{Error, Result};

pub const DEFAULT_COUNT: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Count(usize),
    Bytes(u64),
}

impl Default for Limit {
    fn default() -> Self {
        Limit::Count(DEFAULT_COUNT)
    }
}

fn bad(text: &str) -> Error {
    Error::Usage(format!(
        "bad journal limit {text:?}: use a number of changes (50000, 200k, 1m) or a memory size (512kb, 50mb, 18gb)"
    ))
}

impl Limit {
    pub fn parse(text: &str) -> Result<Self> {
        let t: String = text
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| !matches!(c, ' ' | '_' | ','))
            .collect();
        let split = t.find(|c: char| c.is_alphabetic()).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let n: f64 = num.parse().map_err(|_| bad(text))?;
        if !n.is_finite() || n <= 0.0 {
            return Err(bad(text));
        }
        let (kb, mb, gb) = (1u64 << 10, 1u64 << 20, 1u64 << 30);
        let limit = match unit {
            "" => Limit::Count(n as usize),
            "k" => Limit::Count((n * 1e3) as usize),
            "m" => Limit::Count((n * 1e6) as usize),
            "b" => Limit::Bytes(n as u64),
            "kb" | "kib" => Limit::Bytes((n * kb as f64) as u64),
            "mb" | "mib" => Limit::Bytes((n * mb as f64) as u64),
            "gb" | "gib" => Limit::Bytes((n * gb as f64) as u64),
            _ => return Err(bad(text)),
        };
        match limit {
            Limit::Count(0) | Limit::Bytes(0) => Err(bad(text)),
            l => Ok(l),
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Limit::Count(n) => write!(f, "{n} changes"),
            Limit::Bytes(b) => write!(f, "{b} bytes of memory"),
        }
    }
}

fn file(dir: &Path) -> std::path::PathBuf {
    dir.join("journal-limit.txt")
}

pub fn load(dir: &Path) -> Result<Limit> {
    match std::fs::read_to_string(file(dir)) {
        Ok(text) => Limit::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Limit::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn save(dir: &Path, limit: Limit) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = match limit {
        Limit::Count(n) => n.to_string(),
        Limit::Bytes(b) => format!("{b}b"),
    };
    let tmp = file(dir).with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file(dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_memory_sizes_parse() {
        assert_eq!(Limit::parse("50000").unwrap(), Limit::Count(50_000));
        assert_eq!(Limit::parse("200k").unwrap(), Limit::Count(200_000));
        assert_eq!(Limit::parse("1.5M").unwrap(), Limit::Count(1_500_000));
        assert_eq!(Limit::parse("512kb").unwrap(), Limit::Bytes(512 * 1024));
        assert_eq!(Limit::parse("50 MB").unwrap(), Limit::Bytes(50 << 20));
        assert_eq!(Limit::parse("18gb").unwrap(), Limit::Bytes(18 << 30));
        assert_eq!(Limit::parse("1_000").unwrap(), Limit::Count(1000));
    }

    #[test]
    fn nonsense_is_a_usage_error() {
        for t in ["", "0", "-5", "abc", "10x", "0mb", "k", "nan", "inf"] {
            assert!(matches!(Limit::parse(t), Err(Error::Usage(_))), "{t:?}");
        }
    }

    #[test]
    fn the_limit_is_saved_and_defaults_when_never_set() {
        let d = std::path::PathBuf::from("/var/tmp/au-fw-tests")
            .join(format!("{}-limit", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(load(&d).unwrap(), Limit::default());
        for l in [Limit::Count(200_000), Limit::Bytes(50 << 20)] {
            save(&d, l).unwrap();
            assert_eq!(load(&d).unwrap(), l);
        }
        let _ = std::fs::remove_dir_all(d);
    }
}
