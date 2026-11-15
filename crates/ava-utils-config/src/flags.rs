use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use crate::error::{Error, Result};

#[derive(Debug, Default, Clone)]
pub struct Spec {
    switches: HashSet<String>,
    options: HashSet<String>,
}

impl Spec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn switch(mut self, name: &str) -> Self {
        self.switches.insert(name.to_string());
        self
    }

    pub fn option(mut self, name: &str) -> Self {
        self.options.insert(name.to_string());
        self
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    switches: HashSet<String>,
    options: HashMap<String, String>,
    positional: Vec<String>,
}

impl Args {
    pub fn has(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    pub fn get_parsed<T: FromStr>(&self, name: &str) -> Result<Option<T>> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| Error::Usage(format!("bad value for --{name}: {v:?}"))),
        }
    }

    pub fn positional(&self) -> &[String] {
        &self.positional
    }
}

pub fn parse(spec: &Spec, args: &[String]) -> Result<Args> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            out.positional.extend(it.by_ref().cloned());
            break;
        }
        let Some(flag) = flag_name(arg) else {
            out.positional.push(arg.clone());
            continue;
        };
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag, None),
        };
        if spec.switches.contains(name) {
            if inline.is_some() {
                return Err(Error::Usage(format!("--{name} does not take a value")));
            }
            out.switches.insert(name.to_string());
        } else if spec.options.contains(name) {
            let value = match inline {
                Some(v) => v,
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| Error::Usage(format!("--{name} needs a value")))?,
            };
            out.options.insert(name.to_string(), value);
        } else {
            return Err(Error::Usage(format!("unknown flag: {arg}")));
        }
    }
    Ok(out)
}

fn flag_name(arg: &str) -> Option<&str> {
    if let Some(long) = arg.strip_prefix("--") {
        return (!long.is_empty()).then_some(long);
    }
    let short = arg.strip_prefix('-')?;
    (!short.is_empty()).then_some(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn spec() -> Spec {
        Spec::new()
            .switch("recursive")
            .switch("r")
            .option("out")
            .option("depth")
    }

    #[test]
    fn switches_options_and_positionals() {
        let a = parse(
            &spec(),
            &v(&["scan", "--recursive", "--out", "x.db", "/home"]),
        )
        .unwrap();
        assert!(a.has("recursive"));
        assert_eq!(a.get("out"), Some("x.db"));
        assert_eq!(a.positional(), ["scan", "/home"]);
    }

    #[test]
    fn equals_form_and_short_form() {
        let a = parse(&spec(), &v(&["-r", "--out=y.db"])).unwrap();
        assert!(a.has("r"));
        assert_eq!(a.get("out"), Some("y.db"));
    }

    #[test]
    fn double_dash_ends_flags() {
        let a = parse(&spec(), &v(&["--", "--recursive", "-x"])).unwrap();
        assert!(!a.has("recursive"));
        assert_eq!(a.positional(), ["--recursive", "-x"]);
    }

    #[test]
    fn a_lone_dash_is_a_positional() {
        let a = parse(&spec(), &v(&["-"])).unwrap();
        assert_eq!(a.positional(), ["-"]);
    }

    #[test]
    fn bad_input_is_a_usage_error() {
        for bad in [
            v(&["--nope"]),
            v(&["--out"]),
            v(&["--recursive=1"]),
            v(&["-5"]),
        ] {
            assert!(
                matches!(parse(&spec(), &bad), Err(Error::Usage(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn typed_values() {
        let a = parse(&spec(), &v(&["--depth", "3"])).unwrap();
        assert_eq!(a.get_parsed::<u32>("depth").unwrap(), Some(3));
        let a = parse(&spec(), &v(&["--depth", "x"])).unwrap();
        assert!(a.get_parsed::<u32>("depth").is_err());
        assert_eq!(a.get_parsed::<u32>("out").unwrap(), None);
    }
}
