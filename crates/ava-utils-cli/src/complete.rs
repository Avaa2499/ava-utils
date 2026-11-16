use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::Manifest;

pub const BUILTINS: [&str; 8] = [
    "install",
    "remove",
    "check",
    "help",
    "completions",
    "list",
    "enable",
    "disable",
];

pub const BASH_SCRIPT: &str = "_au() {
    local IFS=$'\\n'
    COMPREPLY=($(au __complete \"${COMP_WORDS[@]:1:COMP_CWORD}\" 2>/dev/null))
}
complete -o default -F _au au
";

pub fn script(shell: &str) -> Result<String> {
    match shell {
        "bash" => Ok(BASH_SCRIPT.to_string()),
        other => Err(Error::Usage(format!(
            "no completion script for {other:?}; supported: bash"
        ))),
    }
}

fn starting_with(prefix: &str, mut items: Vec<String>) -> Vec<String> {
    items.retain(|i| i.starts_with(prefix));
    items.sort();
    items.dedup();
    items
}

pub fn candidates(words: &[String], manifests: &[Manifest], workspace: &[String]) -> Vec<String> {
    let Some((current, before)) = words.split_last() else {
        return Vec::new();
    };
    let (raw, before) = match before.first().map(String::as_str) {
        Some("--raw") => (true, &before[1..]),
        _ => (false, before),
    };
    let installed: Vec<String> = manifests.iter().map(|m| m.module.clone()).collect();
    match before {
        [] => {
            let mut all = installed;
            if !raw {
                all.extend(BUILTINS.iter().map(|b| b.to_string()));
                all.push("--raw".to_string());
            }
            starting_with(current, all)
        }
        [verb] => match verb.as_str() {
            "install" if !raw => starting_with(current, workspace.to_vec()),
            "remove" | "help" | "enable" | "disable" if !raw => starting_with(current, installed),
            "completions" if !raw => starting_with(current, vec!["bash".to_string()]),
            module => manifests
                .iter()
                .find(|m| m.module == module)
                .map(|m| {
                    starting_with(current, m.commands.iter().map(|c| c.name.clone()).collect())
                })
                .unwrap_or_default(),
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(module: &str, commands: &[&str]) -> Manifest {
        let cmds: String = commands
            .iter()
            .map(|c| format!("[[commands]]\nname = \"{c}\"\nhelp = \"h\"\n"))
            .collect();
        Manifest::parse(&format!("module = \"{module}\"\n{cmds}")).unwrap()
    }

    fn fixtures() -> (Vec<Manifest>, Vec<String>) {
        (
            vec![
                manifest("file-indexer", &["scan", "status", "quarantine"]),
                manifest("file-watcher", &["add", "list", "journal"]),
                manifest("web-searcher", &[]),
            ],
            vec!["file-indexer".into(), "file-watcher".into(), "toy".into()],
        )
    }

    fn c(words: &[&str]) -> Vec<String> {
        let (m, w) = fixtures();
        let words: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        candidates(&words, &m, &w)
    }

    #[test]
    fn the_first_word_offers_built_ins_installed_modules_and_raw() {
        assert_eq!(
            c(&[""]),
            [
                "--raw",
                "check",
                "completions",
                "disable",
                "enable",
                "file-indexer",
                "file-watcher",
                "help",
                "install",
                "list",
                "remove",
                "web-searcher"
            ]
        );
    }

    #[test]
    fn a_prefix_narrows_the_first_word() {
        assert_eq!(c(&["i"]), ["install"]);
        assert_eq!(c(&["fi"]), ["file-indexer", "file-watcher"]);
        assert_eq!(c(&["file-w"]), ["file-watcher"]);
        assert_eq!(c(&["--r"]), ["--raw"]);
        assert!(c(&["zzz"]).is_empty());
    }

    #[test]
    fn after_raw_only_modules_are_offered() {
        assert_eq!(
            c(&["--raw", ""]),
            ["file-indexer", "file-watcher", "web-searcher"]
        );
        assert_eq!(c(&["--raw", "file-indexer", "s"]), ["scan", "status"]);
        assert!(
            c(&["--raw", "install", ""]).is_empty()
                || !c(&["--raw", "install", ""]).contains(&"toy".to_string())
        );
    }

    #[test]
    fn install_offers_workspace_modules_and_remove_and_help_offer_installed_ones() {
        assert_eq!(c(&["install", ""]), ["file-indexer", "file-watcher", "toy"]);
        assert_eq!(c(&["install", "t"]), ["toy"]);
        assert_eq!(
            c(&["remove", ""]),
            ["file-indexer", "file-watcher", "web-searcher"]
        );
        assert_eq!(c(&["help", "web"]), ["web-searcher"]);
        assert_eq!(c(&["completions", ""]), ["bash"]);
        assert!(c(&["check", ""]).is_empty());
    }

    #[test]
    fn a_module_offers_its_own_commands_from_the_manifest() {
        assert_eq!(c(&["file-indexer", ""]), ["quarantine", "scan", "status"]);
        assert_eq!(c(&["file-indexer", "sc"]), ["scan"]);
        assert_eq!(c(&["file-watcher", "j"]), ["journal"]);
        assert!(c(&["web-searcher", ""]).is_empty());
    }

    #[test]
    fn unknown_modules_and_later_words_offer_nothing_so_bash_falls_back_to_files() {
        assert!(c(&["nope", ""]).is_empty());
        assert!(c(&["file-indexer", "scan", ""]).is_empty());
        assert!(c(&["help", "file-indexer", ""]).is_empty());
        assert!(candidates(&[], &[], &[]).is_empty());
    }

    #[test]
    fn the_bash_script_calls_the_hidden_command_and_falls_back_to_file_names() {
        let s = script("bash").unwrap();
        assert!(s.contains("au __complete") && s.contains("complete -o default -F _au au"));
        assert!(matches!(script("zsh"), Err(Error::Usage(_))));
    }
}
