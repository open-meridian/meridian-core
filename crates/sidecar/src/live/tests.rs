use super::*;

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meridian-live-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn change(files: &[(&str, &str)], deleted: &[&str]) -> Change {
    Change {
        files: files
            .iter()
            .map(|(path, text)| (path.to_string(), STANDARD.encode(text)))
            .collect(),
        deleted: deleted.iter().map(|d| d.to_string()).collect(),
    }
}

#[test]
fn a_path_is_held_to_the_plugin_and_out_of_its_bookkeeping() {
    for fine in ["src/page.py", "./pyproject.toml", "a/b/c.py"] {
        assert!(held(fine).is_ok(), "{fine}");
    }
    for refused in [
        "",
        "/etc/passwd",
        "../outside.py",
        "src/../../outside.py",
        ".meridian/revision",
        "./.meridian/output.jsonl",
    ] {
        assert!(held(refused).is_err(), "{refused}");
    }
}

#[test]
fn a_change_is_written_then_its_revision_and_a_bad_one_writes_nothing() {
    let dir = folder("apply");
    let live = Live::new(&dir);
    assert_eq!(live.revision(), 0);
    let revision = live
        .apply(change(
            &[
                ("src/page.py", "print('one')"),
                ("pyproject.toml", "[project]"),
            ],
            &[],
        ))
        .unwrap();
    assert_eq!(revision, 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("src/page.py")).unwrap(),
        "print('one')"
    );
    assert_eq!(live.revision(), 1);

    // One bad path among good ones: nothing is written, no revision.
    let refused = live
        .apply(change(
            &[("src/page.py", "print('two')"), ("../escape.py", "x")],
            &[],
        ))
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        std::fs::read_to_string(dir.join("src/page.py")).unwrap(),
        "print('one')"
    );
    assert_eq!(live.revision(), 1);

    // Deleting is part of a change; deleting what is not there is not an error.
    assert_eq!(
        live.apply(change(&[], &["pyproject.toml", "gone.py"]))
            .unwrap(),
        2
    );
    assert!(!dir.join("pyproject.toml").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&dir.join("src/page.py")),
            0o664,
            "the plugin's group may replace it"
        );
        assert_eq!(mode(&dir.join("src")) & 0o070, 0o070);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn output_and_events_are_what_came_after_a_revision_in_order() {
    let dir = folder("read");
    let live = Live::new(&dir);
    std::fs::create_dir_all(dir.join(".meridian")).unwrap();
    std::fs::write(
        dir.join(".meridian/output.jsonl"),
        "{\"revision\":0,\"line\":\"old\"}\n{\"revision\":1,\"line\":\"new\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".meridian/runner-events.jsonl"),
        "{\"revision\":1,\"event\":\"restarted\",\"at\":2.0}\n{\"revision\":1,\"event\":\"ready\",\"at\":3.0}\n",
    )
    .unwrap();
    live.record(1, "synced", serde_json::json!({}));
    let output = live.output_since(Some(0));
    assert_eq!(output["lines"], serde_json::json!(["new"]));
    let events = live.events_since(Some(0))["events"]
        .as_array()
        .unwrap()
        .clone();
    let named: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        named,
        ["restarted", "ready", "synced"],
        "by when they happened"
    );
    assert!(live.events_since(Some(1))["events"]
        .as_array()
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refusal_is_recorded_against_the_revision_running() {
    let dir = folder("refused");
    let live = Live::new(&dir);
    live.apply(change(&[("a.py", "x")], &[])).unwrap();
    live.refused(
        "no grant for platform.street.command.record-statement: this plugin holds no role",
    );
    let events = live.events_since(Some(0))["events"]
        .as_array()
        .unwrap()
        .clone();
    let refused = events.iter().find(|e| e["event"] == "refused").unwrap();
    assert_eq!(refused["revision"], 1);
    assert!(refused["reason"].as_str().unwrap().contains("no grant"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_both_make_an_endpoint() {
    assert!(Live::from_env(Some("/plugin/live".into()), true).is_some());
    assert!(Live::from_env(Some("/plugin/live".into()), false).is_none());
    assert!(Live::from_env(None, true).is_none());
    assert!(Live::from_env(Some(String::new()), true).is_none());
}

#[test]
fn asked_for_nothing_in_particular_it_is_everything_kept() {
    let dir = folder("all");
    let live = Live::new(&dir);
    live.record(0, "restarted", serde_json::json!({}));
    assert_eq!(
        live.events_since(None)["events"].as_array().unwrap().len(),
        1
    );
    assert!(live.events_since(Some(0))["events"]
        .as_array()
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
