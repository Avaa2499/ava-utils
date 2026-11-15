use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::check_module_name;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub message: String,
    pub module: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    pub help: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub module: String,
    #[serde(default)]
    pub answers: Vec<String>,
    #[serde(default)]
    pub sends: Vec<String>,
    #[serde(default)]
    pub calls: Vec<Binding>,
    #[serde(default)]
    pub listens: Vec<Binding>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub loop_partners: Vec<String>,
}

fn bad(m: &str, why: String) -> Error {
    Error::Usage(format!("manifest for {m}: {why}"))
}

fn unique<'a>(m: &str, what: &str, items: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for i in items {
        if !seen.insert(i) {
            return Err(bad(m, format!("duplicate {what}: {i}")));
        }
    }
    Ok(())
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(text)
            .map_err(|e| Error::Usage(format!("manifest is not valid: {}", e.message())))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| Error::Usage(format!("cannot write manifest: {e}")))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let manifest = Self::parse(&text)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem != manifest.module {
            return Err(bad(
                &manifest.module,
                format!(
                    "file is named {stem}.toml but declares module {}",
                    manifest.module
                ),
            ));
        }
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        let m = self.module.as_str();
        check_module_name(m).map_err(|_| bad(m, "bad module name".into()))?;
        for name in self.answers.iter().chain(&self.sends) {
            check_module_name(name).map_err(|_| bad(m, format!("bad message name: {name}")))?;
        }
        for b in self.calls.iter().chain(&self.listens) {
            check_module_name(&b.message)
                .map_err(|_| bad(m, format!("bad message name: {}", b.message)))?;
            check_module_name(&b.module)
                .map_err(|_| bad(m, format!("bad module name: {}", b.module)))?;
            if b.module == m {
                return Err(bad(
                    m,
                    format!("{} is bound to the module itself", b.message),
                ));
            }
        }
        for p in &self.loop_partners {
            check_module_name(p).map_err(|_| bad(m, format!("bad loop partner: {p}")))?;
            if p == m {
                return Err(bad(m, "a module cannot be its own loop partner".into()));
            }
        }
        for c in &self.commands {
            check_module_name(&c.name)
                .map_err(|_| bad(m, format!("bad command name: {}", c.name)))?;
            if c.help.trim().is_empty() {
                return Err(bad(m, format!("command {} has no help text", c.name)));
            }
        }
        unique(m, "answer", self.answers.iter().map(String::as_str))?;
        unique(m, "sent topic", self.sends.iter().map(String::as_str))?;
        unique(m, "call", self.calls.iter().map(|b| b.message.as_str()))?;
        unique(m, "listen", self.listens.iter().map(|b| b.message.as_str()))?;
        unique(m, "command", self.commands.iter().map(|c| c.name.as_str()))?;
        unique(
            m,
            "loop partner",
            self.loop_partners.iter().map(String::as_str),
        )?;
        Ok(())
    }
}

pub fn load_all(dir: &Path) -> Result<Vec<Manifest>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            found.push(Manifest::load(&path)?);
        }
    }
    found.sort_by(|a, b| a.module.cmp(&b.module));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const SEARCHER: &str = r#"
module = "file-searcher"
answers = ["run-command"]

[[calls]]
message = "file-lookup"
module = "file-indexer"

[[commands]]
name = "find"
help = "Search the file index"
"#;

    #[test]
    fn parses_a_manifest() {
        let m = Manifest::parse(SEARCHER).unwrap();
        assert_eq!(m.module, "file-searcher");
        assert_eq!(m.calls[0].module, "file-indexer");
        assert_eq!(m.commands[0].name, "find");
        assert!(m.listens.is_empty() && m.loop_partners.is_empty());
    }

    #[test]
    fn roundtrips_through_toml() {
        let m = Manifest::parse(SEARCHER).unwrap();
        assert_eq!(Manifest::parse(&m.to_toml().unwrap()).unwrap(), m);
    }

    #[test]
    fn typos_and_bad_values_are_refused() {
        let cases = [
            "module = \"x\"\nanswerz = []",
            "module = \"Bad Name\"",
            "module = \"x\"\nanswers = [\"a\", \"a\"]",
            "module = \"x\"\nloop_partners = [\"x\"]",
            "module = \"x\"\n[[calls]]\nmessage = \"m\"\nmodule = \"x\"",
            "module = \"x\"\n[[commands]]\nname = \"c\"\nhelp = \" \"",
            "not toml at all [",
        ];
        for c in cases {
            assert!(matches!(Manifest::parse(c), Err(Error::Usage(_))), "{c}");
        }
    }

    #[test]
    fn load_all_reads_a_folder_and_checks_file_names() {
        let dir = TempDir::new();
        let d = dir.sock("x");
        let folder = d.parent().unwrap().to_path_buf();
        std::fs::write(folder.join("file-searcher.toml"), SEARCHER).unwrap();
        std::fs::write(folder.join("notes.txt"), "ignored").unwrap();
        let all = load_all(&folder).unwrap();
        assert_eq!(all.len(), 1);
        std::fs::write(folder.join("wrong-name.toml"), SEARCHER).unwrap();
        assert!(load_all(&folder).is_err());
    }

    #[test]
    fn a_missing_folder_means_no_manifests() {
        let dir = TempDir::new();
        let missing = dir.sock("x").parent().unwrap().join("nope");
        assert!(load_all(&missing).unwrap().is_empty());
    }
}
