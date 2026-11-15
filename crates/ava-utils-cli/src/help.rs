use ava_utils_config::manifest::Manifest;

const BUILT_IN: &str =
    "au install <module>     build, test, loop-check and install a module from this workspace
au remove <module>      stop and remove a module
au check                check the installed manifests for loops
au [--raw] <module> <args>   run a module's command (--raw shows the reply bytes)
au help [module]        this help";

pub fn help(manifests: &[Manifest], only: Option<&str>) -> String {
    let mut out = String::new();
    if only.is_none() {
        out.push_str(BUILT_IN);
        out.push('\n');
    }
    let mut any = false;
    for m in manifests
        .iter()
        .filter(|m| only.is_none_or(|o| o == m.module))
    {
        any = true;
        out.push_str(&format!("\n{}\n", m.module));
        if m.commands.is_empty() {
            out.push_str("  (no commands listed)\n");
        }
        for c in &m.commands {
            out.push_str(&format!("  au {} {:<12} {}\n", m.module, c.name, c.help));
        }
    }
    if !any && only.is_some() {
        out.push_str(&format!("{} is not installed\n", only.unwrap_or("")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ava_utils_config::manifest::Command;

    fn searcher() -> Manifest {
        Manifest::parse("module = \"file-searcher\"")
            .map(|mut m| {
                m.commands.push(Command {
                    name: "find".into(),
                    help: "Search the file index".into(),
                });
                m
            })
            .unwrap()
    }

    #[test]
    fn lists_built_ins_and_every_module_command() {
        let h = help(&[searcher()], None);
        assert!(h.contains("au install <module>"));
        assert!(h.contains("au file-searcher find"));
        assert!(h.contains("Search the file index"));
    }

    #[test]
    fn one_module_only() {
        let h = help(&[searcher()], Some("file-searcher"));
        assert!(!h.contains("au install"));
        assert!(h.contains("find"));
        assert!(help(&[searcher()], Some("nope")).contains("not installed"));
    }
}
