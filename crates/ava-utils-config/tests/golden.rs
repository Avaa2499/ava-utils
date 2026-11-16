use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::PathBuf;

use ava_utils_config::envelope::{Envelope, Kind};
use ava_utils_config::messages::*;
use rmpv::Value;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    let t = text.trim();
    (0..t.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&t[i..i + 2], 16).expect("fixture is hex"))
        .collect()
}

fn to_value(bytes: &[u8]) -> Value {
    rmpv::decode::read_value(&mut &bytes[..]).expect("fixture is MessagePack")
}

fn to_bytes(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, v).unwrap();
    out
}

fn map_entries(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Map(m) => m
            .iter()
            .map(|(k, v)| (k.as_str().expect("string key").to_string(), v.clone()))
            .collect(),
        other => panic!("not a map: {other:?}"),
    }
}

fn rebuild(entries: Vec<(String, Value)>) -> Vec<u8> {
    to_bytes(&Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    ))
}

fn fixture(name: &str, bytes: &[u8], seen: &mut BTreeSet<String>) -> Vec<u8> {
    seen.insert(format!("{name}.hex"));
    let path = dir().join(format!("{name}.hex"));
    if std::env::var_os("AU_BLESS").is_some() {
        std::fs::write(&path, format!("{}\n", hex(bytes))).unwrap();
    }
    let saved = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing fixture {name}: run with AU_BLESS=1 once to create it")
    });
    assert_eq!(
        saved.trim(),
        hex(bytes),
        "the wire bytes of {name} changed. If that is intended, it is a format change: run with AU_BLESS=1 and review the diff"
    );
    unhex(&saved)
}

fn check<T>(name: &str, full: T, bare: T, optional: &[&str], seen: &mut BTreeSet<String>)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let bytes = rmp_serde::to_vec_named(&full).unwrap();
    let saved = fixture(name, &bytes, seen);

    let old: T = rmp_serde::from_slice(&saved).unwrap();
    assert_eq!(
        old, full,
        "{name}: saved bytes no longer decode to the same value"
    );

    let mut with_extra = map_entries(&to_value(&saved));
    with_extra.push(("a_box_from_the_future".into(), Value::from(7)));
    with_extra.push(("another_one".into(), Value::Array(vec![Value::from("x")])));
    let extra: T = rmp_serde::from_slice(&rebuild(with_extra)).unwrap();
    assert_eq!(extra, full, "{name}: unknown boxes must be ignored");

    let without_optional: Vec<_> = map_entries(&to_value(&saved))
        .into_iter()
        .filter(|(k, _)| !optional.contains(&k.as_str()))
        .collect();
    let stripped: T = rmp_serde::from_slice(&rebuild(without_optional)).unwrap();
    assert_eq!(
        stripped, bare,
        "{name}: optional boxes must be allowed to be missing"
    );

    for (key, _) in map_entries(&to_value(&saved)) {
        if optional.contains(&key.as_str()) {
            continue;
        }
        let missing: Vec<_> = map_entries(&to_value(&saved))
            .into_iter()
            .filter(|(k, _)| *k != key)
            .collect();
        assert!(
            rmp_serde::from_slice::<T>(&rebuild(missing)).is_err(),
            "{name}: a message without its required box `{key}` must be refused"
        );
    }
}

fn envelope(kind: Kind, name: &str, body: Value) -> Envelope {
    Envelope {
        id: "abc123".into(),
        kind,
        name: name.into(),
        body,
        hops: 0,
        fds: 0,
    }
}

fn paths(p: &[&str]) -> Vec<String> {
    p.iter().map(|s| s.to_string()).collect()
}

fn info() -> FileInfo {
    FileInfo {
        path: "/home/a/Report.pdf".into(),
        size: 1234,
        mtime: 1_700_000_000,
        is_dir: false,
    }
}

#[test]
fn every_message_keeps_its_exact_wire_format() {
    let mut seen = BTreeSet::new();
    let s = &mut seen;

    check(
        "run-command",
        RunCommand {
            args: paths(&["scan", "/home/a"]),
        },
        RunCommand {
            args: paths(&["scan", "/home/a"]),
        },
        &[],
        s,
    );
    check(
        "command-output",
        CommandOutput {
            text: "scan started".into(),
            job: Some("7f3a9c".into()),
        },
        CommandOutput {
            text: "scan started".into(),
            job: None,
        },
        &["job"],
        s,
    );
    check(
        "job-progress",
        JobProgress {
            job: "7f3a9c".into(),
        },
        JobProgress {
            job: "7f3a9c".into(),
        },
        &[],
        s,
    );
    for (state, name) in [
        (JobState::Running, "running"),
        (JobState::Paused, "paused"),
        (JobState::Done, "done"),
        (JobState::Cancelled, "cancelled"),
        (JobState::Failed, "failed"),
    ] {
        check(
            &format!("job-status-{name}"),
            JobStatus {
                job: "7f3a9c".into(),
                state,
                percent: Some(50),
                eta_secs: Some(90),
                detail: Some("1000 entries".into()),
            },
            JobStatus {
                job: "7f3a9c".into(),
                state,
                percent: None,
                eta_secs: None,
                detail: None,
            },
            &["percent", "eta_secs", "detail"],
            s,
        );
    }
    for (mode, name) in [(CancelMode::Force, "force"), (CancelMode::Safe, "safe")] {
        check(
            &format!("job-cancel-{name}"),
            JobCancel {
                job: "7f3a9c".into(),
                mode,
            },
            JobCancel {
                job: "7f3a9c".into(),
                mode,
            },
            &[],
            s,
        );
    }
    check(
        "files-changed",
        FilesChanged {
            changed: paths(&["/a/1", "/a/2"]),
            removed: paths(&["/a/3"]),
            ..Default::default()
        },
        FilesChanged::default(),
        &["changed", "removed"],
        s,
    );
    check(
        "files-changed-cursor",
        FilesChanged {
            changed: paths(&["/a/1"]),
            removed: vec![],
            epoch: 0x1122_3344_5566_7788,
            seq: 42,
            first_seq: 7,
            roots: paths(&["/a"]),
        },
        FilesChanged::default(),
        &["changed", "removed", "epoch", "seq", "first_seq", "roots"],
        s,
    );
    check(
        "file-lookup",
        FileLookup {
            name_contains: Some("report".into()),
            ext: Some("pdf".into()),
            under: Some("/home/a".into()),
            limit: Some(50),
        },
        FileLookup::default(),
        &["name_contains", "ext", "under", "limit"],
        s,
    );
    check("file-info", info(), info(), &[], s);
    check(
        "file-lookup-result",
        FileLookupResult {
            files: vec![info()],
            truncated: true,
        },
        FileLookupResult {
            files: vec![info()],
            truncated: false,
        },
        &["truncated"],
        s,
    );
    check(
        "search",
        Search {
            pattern: "*.rs".into(),
            under: Some("/home/a".into()),
            limit: Some(10),
        },
        Search {
            pattern: "*.rs".into(),
            under: None,
            limit: None,
        },
        &["under", "limit"],
        s,
    );
    check(
        "search-result",
        SearchResult {
            files: vec![info()],
            truncated: true,
        },
        SearchResult {
            files: vec![info()],
            truncated: false,
        },
        &["truncated"],
        s,
    );
    check(
        "watch-set-roots",
        WatchSetRoots {
            roots: paths(&["/home/a/Desktop"]),
        },
        WatchSetRoots {
            roots: paths(&["/home/a/Desktop"]),
        },
        &[],
        s,
    );

    let call = envelope(Kind::Call, "file-lookup", Value::Nil);
    for (kind, name) in [
        (Kind::Call, "call"),
        (Kind::Reply, "reply"),
        (Kind::Event, "event"),
    ] {
        let body = Value::Map(vec![(Value::from("n"), Value::from(1))]);
        check(
            &format!("envelope-{name}"),
            envelope(kind, "files-changed", body),
            envelope(kind, "files-changed", Value::Nil),
            &["body"],
            s,
        );
    }
    check(
        "envelope-event-hops",
        Envelope {
            hops: 3,
            ..envelope(Kind::Event, "files-changed", Value::Nil)
        },
        envelope(Kind::Event, "files-changed", Value::Nil),
        &["body", "hops"],
        s,
    );
    check(
        "envelope-call-fds",
        Envelope {
            fds: 2,
            ..envelope(Kind::Call, "files-changed", Value::Nil)
        },
        envelope(Kind::Call, "files-changed", Value::Nil),
        &["body", "fds"],
        s,
    );
    let error = Envelope::error(&call, "bad-request", "usage: scan <folder>").unwrap();
    check(
        "envelope-error",
        error.clone(),
        Envelope {
            body: Value::Nil,
            ..error
        },
        &["body"],
        s,
    );

    let on_disk: BTreeSet<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        on_disk, seen,
        "fixture files and checked messages must match (delete unused fixtures)"
    );
}

#[test]
fn error_replies_carry_a_code_and_a_message_box() {
    let call = envelope(Kind::Call, "x", Value::Nil);
    let e = Envelope::error(&call, "failed", "boom").unwrap();
    let boxes: Vec<String> = map_entries(&e.body).into_iter().map(|(k, _)| k).collect();
    assert_eq!(boxes, ["code", "message"]);
}
