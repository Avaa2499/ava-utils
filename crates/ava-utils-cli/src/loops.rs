use std::collections::{BTreeMap, BTreeSet};

use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::Manifest;

type Edges = BTreeSet<(String, String)>;

fn refuse(why: String) -> Error {
    Error::Usage(why)
}

fn edges(manifests: &[Manifest]) -> Edges {
    let mut out = Edges::new();
    for m in manifests {
        for b in &m.calls {
            out.insert((m.module.clone(), b.module.clone()));
        }
        for b in &m.listens {
            out.insert((b.module.clone(), m.module.clone()));
        }
    }
    out
}

fn mutual_partners(manifests: &[Manifest]) -> Result<BTreeSet<(String, String)>> {
    let by_name: BTreeMap<&str, &Manifest> =
        manifests.iter().map(|m| (m.module.as_str(), m)).collect();
    let mut pairs = BTreeSet::new();
    for m in manifests {
        for p in &m.loop_partners {
            let Some(other) = by_name.get(p.as_str()) else {
                continue;
            };
            if !other.loop_partners.contains(&m.module) {
                return Err(refuse(format!(
                    "{} names {p} as a loop partner, but {p} does not name {}",
                    m.module, m.module
                )));
            }
            pairs.insert((m.module.clone(), p.clone()));
        }
    }
    Ok(pairs)
}

pub fn check(manifests: &[Manifest]) -> Result<()> {
    let partners = mutual_partners(manifests)?;
    let mut edges = edges(manifests);
    for (a, b) in &partners {
        let there = (a.clone(), b.clone());
        let back = (b.clone(), a.clone());
        if edges.contains(&there) && edges.contains(&back) {
            edges.remove(&there);
            edges.remove(&back);
        }
    }
    find_cycle(&edges).map_or(Ok(()), |c| Err(refuse(format!("loop: {}", c.join(" -> ")))))
}

fn find_cycle(edges: &Edges) -> Option<Vec<String>> {
    let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (a, b) in edges {
        next.entry(a.as_str()).or_default().push(b.as_str());
        next.entry(b.as_str()).or_default();
    }
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for start in next.keys().copied() {
        let mut path: Vec<&str> = Vec::new();
        if let Some(c) = visit(start, &next, &mut done, &mut path) {
            return Some(c);
        }
    }
    None
}

fn visit<'a>(
    node: &'a str,
    next: &BTreeMap<&'a str, Vec<&'a str>>,
    done: &mut BTreeSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    if let Some(at) = path.iter().position(|p| *p == node) {
        let mut cycle: Vec<String> = path[at..].iter().map(|s| s.to_string()).collect();
        cycle.push(node.to_string());
        return Some(cycle);
    }
    if done.contains(node) {
        return None;
    }
    path.push(node);
    for n in &next[node] {
        if let Some(c) = visit(n, next, done, path) {
            return Some(c);
        }
    }
    path.pop();
    done.insert(node);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ava_utils_config::manifest::Binding;

    fn b(message: &str, module: &str) -> Binding {
        Binding {
            message: message.into(),
            module: module.into(),
        }
    }

    fn m(
        module: &str,
        calls: &[(&str, &str)],
        listens: &[(&str, &str)],
        partners: &[&str],
    ) -> Manifest {
        Manifest {
            module: module.into(),
            answers: vec![],
            sends: vec![],
            calls: calls.iter().map(|(a, c)| b(a, c)).collect(),
            listens: listens.iter().map(|(a, c)| b(a, c)).collect(),
            commands: vec![],
            loop_partners: partners.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_planned_first_modules_have_no_loops() {
        let set = [
            m(
                "file-searcher",
                &[("index-lookup", "file-indexer")],
                &[],
                &[],
            ),
            m(
                "file-indexer",
                &[],
                &[("files-changed", "file-watcher")],
                &[],
            ),
            m("file-watcher", &[], &[], &[]),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn a_two_module_call_loop_is_refused() {
        let set = [
            m("a", &[("x", "b")], &[], &[]),
            m("b", &[("y", "a")], &[], &[]),
        ];
        let e = check(&set).unwrap_err().to_string();
        assert!(e.contains("loop:"), "{e}");
    }

    #[test]
    fn an_event_loop_counts_too() {
        let set = [
            m(
                "file-watcher",
                &[],
                &[("files-changed", "file-indexer")],
                &[],
            ),
            m(
                "file-indexer",
                &[],
                &[("files-changed", "file-watcher")],
                &[],
            ),
        ];
        assert!(check(&set).is_err());
    }

    #[test]
    fn calling_the_producer_you_listen_to_is_a_loop() {
        let set = [
            m("file-watcher", &[], &[], &[]),
            m(
                "file-indexer",
                &[("set-roots", "file-watcher")],
                &[("files-changed", "file-watcher")],
                &[],
            ),
        ];
        assert!(check(&set).is_err());
    }

    #[test]
    fn watcher_events_into_the_indexer_are_not_a_loop() {
        let set = [
            m("file-watcher", &[], &[], &[]),
            m(
                "file-indexer",
                &[],
                &[("files-changed", "file-watcher")],
                &[],
            ),
            m(
                "file-cli-like",
                &[("run-command", "file-watcher")],
                &[],
                &[],
            ),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn a_long_loop_is_refused() {
        let set = [
            m("a", &[("x", "b")], &[], &[]),
            m("b", &[("x", "c")], &[], &[]),
            m("c", &[("x", "a")], &[], &[]),
        ];
        let e = check(&set).unwrap_err().to_string();
        assert!(e.contains("a -> b -> c -> a"), "{e}");
    }

    #[test]
    fn a_two_module_loop_is_allowed_when_both_name_each_other() {
        let set = [
            m("queue", &[("next", "player")], &[], &["player"]),
            m("player", &[("more", "queue")], &[], &["queue"]),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn one_sided_naming_is_refused_even_without_a_loop() {
        let set = [
            m("queue", &[], &[], &["player"]),
            m("player", &[], &[], &[]),
        ];
        let e = check(&set).unwrap_err().to_string();
        assert!(e.contains("does not name"), "{e}");
    }

    #[test]
    fn one_module_can_have_several_loop_partners() {
        let set = [
            m("hub", &[("x", "a"), ("y", "b")], &[], &["a", "b"]),
            m("a", &[("x", "hub")], &[], &["hub"]),
            m("b", &[("y", "hub")], &[], &["hub"]),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn partners_cannot_hide_a_longer_loop() {
        let set = [
            m("a", &[("x", "b")], &[], &["b"]),
            m("b", &[("x", "c")], &[], &["a"]),
            m("c", &[("x", "a")], &[], &[]),
        ];
        assert!(check(&set).is_err());
    }

    #[test]
    fn partners_do_not_excuse_a_loop_through_a_third_module() {
        let set = [
            m("a", &[("x", "b")], &[], &["b"]),
            m("b", &[("x", "a"), ("y", "c")], &[], &["a"]),
            m("c", &[("z", "b")], &[], &[]),
        ];
        assert!(check(&set).is_err());
    }
}
