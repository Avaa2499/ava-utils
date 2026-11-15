use std::path::Path;

pub const VIRTUAL_ROOTS: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp"];
pub const DEFAULT_EXCLUSIONS: &[&str] =
    &["node_modules/", "target/", ".git/", "venv/", "__pycache__/"];

pub fn is_virtual(path: &Path) -> bool {
    VIRTUAL_ROOTS.iter().any(|v| path.starts_with(v))
}

pub fn is_excluded(path: &str, is_dir: bool, exclusions: &[String]) -> bool {
    let mut components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let last = components.pop();
    exclusions.iter().any(|e| {
        if let Some(dir_pattern) = e.strip_suffix('/') {
            components.contains(&dir_pattern) || (is_dir && last == Some(dir_pattern))
        } else {
            last == Some(e.as_str())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn virtual_filesystems_are_skipped() {
        assert!(is_virtual(Path::new("/proc/1/status")));
        assert!(is_virtual(Path::new("/sys")));
        assert!(!is_virtual(Path::new("/home/ava/proc")));
        assert!(!is_virtual(Path::new("/processing")));
    }

    #[test]
    fn directory_patterns_match_any_ancestor_or_the_directory_itself() {
        let e = ex(&["node_modules/"]);
        assert!(is_excluded("/p/node_modules/x/y.js", false, &e));
        assert!(is_excluded("/p/node_modules", true, &e));
        assert!(!is_excluded("/p/node_modules", false, &e));
        assert!(!is_excluded("/p/src/main.rs", false, &e));
    }

    #[test]
    fn filename_patterns_match_only_the_final_component() {
        let e = ex(&[".DS_Store"]);
        assert!(is_excluded("/p/.DS_Store", false, &e));
        assert!(!is_excluded("/p/.DS_Store/inner", false, &e));
    }

    #[test]
    fn defaults_cover_the_usual_build_folders() {
        let e: Vec<String> = DEFAULT_EXCLUSIONS.iter().map(|s| s.to_string()).collect();
        assert!(is_excluded("/p/target/debug/x", false, &e));
        assert!(is_excluded("/p/.git", true, &e));
    }
}
