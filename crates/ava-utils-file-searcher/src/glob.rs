#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    pattern: Vec<char>,
    whole_path: bool,
}

impl Glob {
    pub fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_lowercase().chars().collect(),
            whole_path: pattern.contains('/'),
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        let subject = if self.whole_path {
            path
        } else {
            path.rsplit('/').next().unwrap_or(path)
        };
        let text: Vec<char> = subject.to_lowercase().chars().collect();
        let (p, t) = (&self.pattern, &text);
        let (mut pi, mut ti) = (0, 0);
        let (mut star, mut mark) = (None, 0);
        while ti < t.len() {
            if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
                pi += 1;
                ti += 1;
            } else if pi < p.len() && p[pi] == '*' {
                star = Some(pi);
                mark = ti;
                pi += 1;
            } else if let Some(s) = star {
                pi = s + 1;
                mark += 1;
                ti = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }

    pub fn literal_ext(&self) -> Option<String> {
        let rest: String = self.pattern.iter().collect::<String>();
        let ext = rest.strip_prefix("*.")?;
        (!self.whole_path && !ext.is_empty() && !ext.contains(['*', '?', '.']))
            .then(|| ext.to_string())
    }

    pub fn longest_literal(&self) -> Option<String> {
        let s: String = self.pattern.iter().collect();
        s.split(['*', '?'])
            .filter(|part| !part.contains('/'))
            .max_by_key(|part| part.len())
            .filter(|part| !part.is_empty())
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_question_mark_match_file_names() {
        assert!(Glob::new("*.rs").matches("/a/b/main.rs"));
        assert!(!Glob::new("*.rs").matches("/a/b/main.rs.bak"));
        assert!(Glob::new("ma?n.*").matches("/x/main.toml"));
        assert!(Glob::new("*").matches("/x/anything"));
        assert!(!Glob::new("a*b").matches("/x/ac"));
        assert!(Glob::new("a*b*c").matches("/x/aXXbYYc"));
    }

    #[test]
    fn matching_ignores_case_and_folders_in_the_path() {
        assert!(Glob::new("*.PDF").matches("/Docs.rs/report.pdf"));
        assert!(!Glob::new("docs*").matches("/docs/readme.md"));
    }

    #[test]
    fn a_slash_in_the_pattern_matches_the_whole_path() {
        assert!(Glob::new("*/src/*.rs").matches("/p/src/lib.rs"));
        assert!(!Glob::new("*/src/*.rs").matches("/p/tests/lib.rs"));
    }

    #[test]
    fn extension_and_literal_hints() {
        assert_eq!(Glob::new("*.RS").literal_ext().as_deref(), Some("rs"));
        assert_eq!(Glob::new("*.tar.gz").literal_ext(), None);
        assert_eq!(Glob::new("report*").literal_ext(), None);
        assert_eq!(
            Glob::new("re*port_final*.txt").longest_literal().as_deref(),
            Some("port_final")
        );
        assert_eq!(Glob::new("*").longest_literal(), None);
    }
}
