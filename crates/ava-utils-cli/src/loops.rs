use std::collections::{BTreeMap, BTreeSet};

use ava_utils_config::error::{Error, Result};
use ava_utils_config::manifest::Manifest;

type Edges = BTreeSet<(String, String)>;

fn refuse(why: String) -> Error {
    Error::Usage(why)
}

fn call_edges(manifests: &[Manifest]) -> Edges {
    manifests
        .iter()
        .flat_map(|m| m.calls.iter().map(|b| (m.module.clone(), b.module.clone())))
        .collect()
}

fn event_edges(manifests: &[Manifest]) -> Edges {
    manifests
        .iter()
        .flat_map(|m| {
            m.listens
                .iter()
                .map(|b| (b.module.clone(), m.module.clone()))
        })
        .collect()
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

fn reachable<'a>(from: &'a str, next: &BTreeMap<&'a str, Vec<&'a str>>) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![from];
    while let Some(n) = todo.pop() {
        for m in next.get(n).into_iter().flatten() {
            if seen.insert(*m) {
                todo.push(m);
            }
        }
    }
    seen
}

fn loop_groups(edges: &Edges) -> Vec<BTreeSet<String>> {
    let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (a, b) in edges {
        next.entry(a.as_str()).or_default().push(b.as_str());
    }
    let reach: BTreeMap<&str, BTreeSet<&str>> =
        next.keys().map(|n| (*n, reachable(n, &next))).collect();
    let mut groups: Vec<BTreeSet<String>> = Vec::new();
    for (node, from_node) in &reach {
        if !from_node.contains(node) || groups.iter().any(|g| g.contains(*node)) {
            continue;
        }
        let group: BTreeSet<String> = from_node
            .iter()
            .filter(|o| reach.get(*o).is_some_and(|r| r.contains(node)))
            .map(|s| s.to_string())
            .chain([node.to_string()])
            .collect();
        groups.push(group);
    }
    groups
}

fn names_everyone_else(manifests: &[Manifest], group: &BTreeSet<String>) -> bool {
    group.iter().all(|member| {
        manifests
            .iter()
            .find(|m| &m.module == member)
            .is_some_and(|m| {
                group
                    .iter()
                    .filter(|o| *o != member)
                    .all(|o| m.loop_partners.contains(o))
            })
    })
}

pub fn check(manifests: &[Manifest]) -> Result<()> {
    let partners = mutual_partners(manifests)?;
    let mut calls = call_edges(manifests);
    let mut events = event_edges(manifests);
    for (a, b) in &partners {
        let there = (a.clone(), b.clone());
        let back = (b.clone(), a.clone());
        if calls.contains(&there) && calls.contains(&back) {
            calls.remove(&there);
            calls.remove(&back);
        }
    }
    if let Some(c) = find_cycle(&calls) {
        return Err(refuse(format!(
            "loop: {} (calls wait on each other, so a loop made only of calls can deadlock; only two modules that name each other may have one)",
            c.join(" -> ")
        )));
    }
    events.extend(calls);
    for group in loop_groups(&events) {
        if !names_everyone_else(manifests, &group) {
            let inside: Edges = events
                .iter()
                .filter(|(a, b)| group.contains(a) && group.contains(b))
                .cloned()
                .collect();
            let path = find_cycle(&inside).unwrap_or_else(|| group.iter().cloned().collect());
            return Err(refuse(format!(
                "loop: {} (a loop that includes events is allowed only if every module in it names all the others in loop_partners)",
                path.join(" -> ")
            )));
        }
    }
    Ok(())
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

    fn gui_set(gui: &[&str], watcher: &[&str], indexer: &[&str]) -> [Manifest; 3] {
        [
            m(
                "gui",
                &[("watch-set-roots", "file-watcher")],
                &[("index-changed", "file-indexer")],
                gui,
            ),
            m("file-watcher", &[], &[], watcher),
            m(
                "file-indexer",
                &[],
                &[("files-changed", "file-watcher")],
                indexer,
            ),
        ]
    }

    #[test]
    fn the_gui_loop_is_refused_until_every_module_in_it_names_the_others() {
        let e = check(&gui_set(&[], &[], &[])).unwrap_err().to_string();
        assert!(e.contains("loop:") && e.contains("loop_partners"), "{e}");
        assert!(check(&gui_set(&["file-watcher", "file-indexer"], &[], &[])).is_err());
        assert!(
            check(&gui_set(
                &["file-watcher", "file-indexer"],
                &["gui", "file-indexer"],
                &["gui"],
            ))
            .is_err(),
            "the indexer must name the watcher too"
        );
    }

    #[test]
    fn the_gui_loop_is_allowed_when_all_three_name_each_other() {
        let set = gui_set(
            &["file-watcher", "file-indexer"],
            &["gui", "file-indexer"],
            &["gui", "file-watcher"],
        );
        assert!(check(&set).is_ok());
    }

    #[test]
    fn naming_everyone_does_not_excuse_a_loop_made_only_of_calls() {
        let set = [
            m("a", &[("x", "b")], &[], &["b", "c"]),
            m("b", &[("x", "c")], &[], &["a", "c"]),
            m("c", &[("x", "a")], &[], &["a", "b"]),
        ];
        let e = check(&set).unwrap_err().to_string();
        assert!(
            e.contains("a -> b -> c -> a") && e.contains("deadlock"),
            "{e}"
        );
    }

    #[test]
    fn an_event_loop_between_two_modules_is_allowed_when_both_name_each_other() {
        let set = [
            m("a", &[], &[("e", "b")], &["b"]),
            m("b", &[], &[("e", "a")], &["a"]),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn a_four_module_event_loop_needs_everyone_to_name_everyone() {
        let names = ["a", "b", "c", "d"];
        let build = |skip: Option<(&str, &str)>| -> Vec<Manifest> {
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let from = names[(i + 3) % 4];
                    let partners: Vec<&str> = names
                        .iter()
                        .copied()
                        .filter(|o| o != n && skip != Some((n, o)) && skip != Some((o, n)))
                        .collect();
                    m(n, &[], &[("e", from)], &partners)
                })
                .collect()
        };
        assert!(check(&build(None)).is_ok());
        let e = check(&build(Some(("a", "c")))).unwrap_err().to_string();
        assert!(e.contains("loop:"), "{e}");
    }

    #[test]
    fn a_member_of_an_allowed_loop_does_not_excuse_a_separate_loop() {
        let set = [
            m("a", &[], &[("e", "b")], &["b"]),
            m("b", &[], &[("e", "a")], &["a"]),
            m("x", &[], &[("e", "y")], &[]),
            m("y", &[], &[("e", "x")], &[]),
        ];
        let e = check(&set).unwrap_err().to_string();
        assert!(e.contains('x') && e.contains('y'), "{e}");
    }

    #[test]
    fn modules_that_only_feed_into_an_allowed_loop_are_not_part_of_it() {
        let set = [
            m("a", &[], &[("e", "b")], &["b"]),
            m("b", &[], &[("e", "a")], &["a"]),
            m("feeder", &[("c", "a")], &[], &[]),
            m("viewer", &[], &[("e", "a")], &[]),
        ];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn partners_named_but_no_loop_is_fine() {
        let set = [m("a", &[], &[], &["b"]), m("b", &[], &[], &["a"])];
        assert!(check(&set).is_ok());
    }

    #[test]
    fn an_empty_set_has_no_loops() {
        assert!(check(&[]).is_ok());
    }
}
