use super::*;
use crate::model::{Direction, Layout};
use serde_json::json;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use wezterm_term::{Terminal, TerminalSize};

fn snapshot() -> Snapshot {
    Snapshot::parse(&serde_json::to_vec(&json!({
        "version": 3, "active": 0,
        "workspaces": [{ "custom_name": "example", "identity_cwd": "/home/user/example", "active_tab": 1,
            "tabs": [{ "custom_name": "build", "focused": 2, "zoomed": true,
                "layout": {"Split": {"direction": "Horizontal", "ratio": 0.3, "first": {"Pane": 1}, "second": {"Pane": 2}}},
                "panes": {"1": {"cwd": "/home/user/example", "label": "shell", "launch_argv": ["must-not-run"]},
                    "2": {"cwd": "/home/user/example/sub", "agent_resume": {"argv": ["must-not-resume"]}}}},
                {"custom_name": "logs", "layout": {"Pane": 3}, "panes": {"3": {"cwd": "/home/user/example"}}}]
        }]
    })).unwrap()).unwrap()
}

#[test]
fn live_preview_shows_titles_and_current_directories_in_their_projects() {
    let workspaces = [
        json!({"workspace_id": "w1", "label": "example"}),
        json!({"workspace_id": "w2", "label": "other"}),
    ];
    let tabs = [
        json!({"workspace_id": "w2", "tab_id": "t2", "label": "other tab"}),
        json!({"workspace_id": "w1", "tab_id": "t1", "label": "Development"}),
    ];
    let panes = [
        json!({"workspace_id": "w1", "tab_id": "t1", "title": "Building example", "label": "worker",
            "cwd": "/home/user/example", "foreground_cwd": "/home/user/example/build"}),
        json!({"workspace_id": "w2", "tab_id": "t2", "title": "Server logs"}),
        json!({"workspace_id": "w1", "tab_id": "t1", "title": " ", "label": "shell", "cwd": "/home/user/example"}),
    ];
    let projects = source::live_projects(&workspaces, &tabs, &panes);
    assert_eq!((projects[0].tabs, projects[0].panes), (1, 2));
    let terminal = &projects[0].terminals[0];
    assert_eq!(terminal.title.as_deref(), Some("Building example"));
    assert_eq!(terminal.tab_name.as_deref(), Some("Development"));
    assert_eq!(terminal.cwd.as_deref(), Some("/home/user/example/build"));
    assert_eq!(projects[0].terminals[1].title.as_deref(), Some("shell"));
    assert_eq!(
        projects[1].terminals[0].title.as_deref(),
        Some("Server logs")
    );
    assert_eq!(
        projects[1].terminals[0].tab_name.as_deref(),
        Some("other tab")
    );
}

#[test]
fn live_preview_handles_missing_and_unbounded_terminal_metadata() {
    let workspaces = [json!({"workspace_id": "w1", "label": "example"})];
    let panes = [
        json!({"workspace_id": "w1", "terminal_title_stripped": " Tests\npassed ", "terminal_title": "raw title"}),
        json!({"workspace_id": "w1", "terminal_title": "x".repeat(300)}),
        json!({"workspace_id": "w1", "cwd": "/home/user/example"}),
    ];
    let projects = source::live_projects(&workspaces, &[], &panes);
    assert_eq!(projects[0].panes, 3);
    assert_eq!(
        projects[0].terminals[0].title.as_deref(),
        Some("Tests passed")
    );
    let long_title = projects[0].terminals[1].title.as_ref().unwrap();
    assert_eq!(long_title.chars().count(), 257);
    assert!(long_title.ends_with('…'));
    assert!(projects[0].terminals[2].title.is_none());
    assert!(projects[0].terminals[2].tab_name.is_none());
    assert_eq!(
        projects[0].terminals[2].cwd.as_deref(),
        Some("/home/user/example")
    );
}

#[test]
fn saved_preview_preserves_terminal_labels_and_layout_order() {
    let mut snapshot = snapshot();
    snapshot.workspaces[0].tabs[0].layout = Layout::Split {
        direction: Direction::Horizontal,
        ratio: 0.5,
        first: Box::new(Layout::Pane(2)),
        second: Box::new(Layout::Pane(1)),
    };
    let projects = source::saved_projects(&snapshot);
    assert_eq!((projects[0].tabs, projects[0].panes), (2, 3));
    let terminals = &projects[0].terminals;
    assert!(terminals[0].title.is_none());
    assert_eq!(terminals[0].tab_name.as_deref(), Some("build"));
    assert_eq!(terminals[0].cwd.as_deref(), Some("/home/user/example/sub"));
    assert_eq!(terminals[1].title.as_deref(), Some("shell"));
    assert_eq!(terminals[2].tab_name.as_deref(), Some("logs"));
}

#[test]
fn saved_working_directories_are_rechecked_after_preview() {
    let directory = tempfile::tempdir().unwrap();
    let mut snapshot = snapshot();
    for pane in snapshot
        .workspaces
        .iter_mut()
        .flat_map(|workspace| &mut workspace.tabs)
        .flat_map(|tab| tab.panes.values_mut())
    {
        pane.cwd = directory.path().to_str().unwrap().to_string();
    }
    snapshot.check_working_directories().unwrap();
    directory.close().unwrap();
    assert!(snapshot
        .check_working_directories()
        .unwrap_err()
        .to_string()
        .contains("working directory is unavailable"));
}

#[test]
fn legacy_and_invalid_snapshots() {
    let legacy = json!({"workspaces": [{"layout": {"Pane": 4}, "panes": {"4": {"cwd": "/home/user"}}, "zoomed": false}]});
    let legacy = Snapshot::parse(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(legacy.pane_count(), 1);
    assert_eq!(legacy.workspaces[0].identity_cwd, "/home/user");
    let mut snapshot = snapshot();
    snapshot.version = 99;
    assert!(snapshot.validate().is_err());
    snapshot.version = 3;
    snapshot.workspaces[0].tabs[1].layout = Layout::Pane(1);
    assert!(snapshot.validate().is_err());
    snapshot.workspaces[0].tabs[1].layout = Layout::Pane(3);
    snapshot.workspaces[0].tabs[1]
        .panes
        .get_mut(&3)
        .unwrap()
        .cwd = "relative".into();
    assert!(snapshot.validate().is_err());
}

#[test]
fn received_fd_batches_include_the_sixty_fifth_pane() {
    use rustix::net::{sendmsg, SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let pty = nix::pty::openpty(None, None).unwrap();
    let (sender, receiver) = UnixStream::pair().unwrap();
    let sending = std::thread::spawn(move || {
        for count in [64, 1] {
            let fds: Vec<_> = (0..count).map(|_| pty.master.as_fd()).collect();
            let mut storage = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(64))];
            let mut ancillary = SendAncillaryBuffer::new(&mut storage);
            assert!(ancillary.push(SendAncillaryMessage::ScmRights(&fds)));
            sendmsg(
                &sender,
                &[std::io::IoSlice::new(b"F")],
                &mut ancillary,
                SendFlags::empty(),
            )
            .unwrap();
        }
    });
    let fds = transport::receive_fds(&receiver, 65).unwrap();
    assert_eq!(fds.len(), 65);
    assert!(fds.iter().all(|fd| rustix::io::fcntl_getfd(fd)
        .unwrap()
        .contains(rustix::io::FdFlags::CLOEXEC)));
    sending.join().unwrap();
}

#[test]
fn json_reader_leaves_descriptor_marker_unconsumed() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    sender.write_all(b"{}\nF").unwrap();
    assert_eq!(source::read_line(&mut receiver, 16).unwrap(), b"{}");
    let mut marker = [0];
    receiver.read_exact(&mut marker).unwrap();
    assert_eq!(marker, [b'F']);
}

#[test]
fn source_manifest_requires_every_layout_pane() {
    let manifest = model::Manifest {
        version: 1,
        source_version: "0.9.3".into(),
        snapshot: snapshot(),
        panes: vec![],
    };
    assert!(manifest
        .validate()
        .unwrap_err()
        .to_string()
        .contains("no live terminal"));
}

#[test]
fn live_fingerprint_is_independent_of_json_map_order() {
    let first = serde_json::from_str(r#"{"workspaces":[["w1","example"]],"tabs":[["t1","w1","shell"]],"panes":[["p1","term1","t1"]]}"#).unwrap();
    let reordered = serde_json::from_str(r#"{"panes":[["p1","term1","t1"]],"tabs":[["t1","w1","shell"]],"workspaces":[["w1","example"]]}"#).unwrap();
    assert_eq!(
        source::live_fingerprint(first).unwrap(),
        source::live_fingerprint(reordered).unwrap()
    );
}

#[test]
fn restores_negotiated_keyboard_paste_and_mouse_state() {
    use wezterm_term::SnapshotKeyboardEncoding;
    let runtime: Runtime = serde_json::from_value(json!({
        "pane_id": 1, "child_pid": 123, "rows": 24, "cols": 80,
        "keyboard_protocol_flags": 3,
        "input_state": {"alternate_screen": true, "application_cursor": true,
            "bracketed_paste": true, "focus_reporting": true,
            "mouse_protocol_mode": "ButtonMotion", "mouse_protocol_encoding": "Sgr"}
    }))
    .unwrap();
    let config = config::ConfigHandle::default_config();
    assert!(!config.enable_kitty_keyboard);
    let mut terminal = Terminal::new(
        TerminalSize {
            rows: runtime.rows as usize,
            cols: runtime.cols as usize,
            ..TerminalSize::default()
        },
        seed_config(config),
        "ThinkTerm",
        "test",
        Box::new(std::io::sink()),
    );
    terminal.advance_bytes(seed::seed_ansi(&runtime).as_bytes());
    let state = terminal.snapshot();
    assert!(state.alt_screen_is_active);
    assert!(state.modes.application_cursor_keys);
    assert!(state.modes.bracketed_paste);
    assert!(state.modes.focus_tracking);
    assert!(state.modes.button_event_mouse);
    assert_eq!(
        state.alt_screen.keyboard_stack.last(),
        Some(&SnapshotKeyboardEncoding::Kitty(3))
    );
    let mut restored = Terminal::new(
        state.size,
        Arc::new(config::TermConfig::with_config(
            config::ConfigHandle::default_config(),
        )),
        "ThinkTerm",
        "test",
        Box::new(std::io::sink()),
    );
    restored.restore(state).unwrap();
    assert_eq!(
        SnapshotKeyboardEncoding::from(restored.get_keyboard_encoding()),
        SnapshotKeyboardEncoding::Kitty(3)
    );
    for (ansi, expected) in [
        ("\x1b[=1u", SnapshotKeyboardEncoding::Kitty(1)),
        ("\x1b[>3u", SnapshotKeyboardEncoding::Kitty(3)),
        ("\x1b[<u", SnapshotKeyboardEncoding::Kitty(1)),
        ("\x1b[<u", SnapshotKeyboardEncoding::Xterm),
        // Once the imported protocol is finished, the user's default
        // applies to subsequent programs again.
        ("\x1b[>3u", SnapshotKeyboardEncoding::Xterm),
    ] {
        restored.advance_bytes(ansi);
        assert_eq!(
            SnapshotKeyboardEncoding::from(restored.get_keyboard_encoding()),
            expected
        );
    }
}

#[test]
fn restores_each_mouse_encoding() {
    for (source, expected) in [
        ("Utf8", "Utf8"),
        ("Sgr", "SGR"),
        ("SgrPixels", "SgrPixels"),
        ("X10", "X10"),
    ] {
        let runtime: Runtime = serde_json::from_value(json!({
            "pane_id": 1, "child_pid": 123, "rows": 24, "cols": 80,
            "input_state": {"mouse_protocol_mode": "ButtonMotion", "mouse_protocol_encoding": source}
        })).unwrap();
        let mut terminal = Terminal::new(
            TerminalSize {
                rows: runtime.rows as usize,
                cols: runtime.cols as usize,
                ..TerminalSize::default()
            },
            seed_config(config::ConfigHandle::default_config()),
            "ThinkTerm",
            "test",
            Box::new(std::io::sink()),
        );
        terminal.advance_bytes(seed::seed_ansi(&runtime).as_bytes());
        assert_eq!(
            serde_json::to_value(terminal.snapshot().modes.mouse_encoding).unwrap(),
            json!(expected)
        );
    }
}

fn seed_config(config: config::ConfigHandle) -> Arc<config::TermConfig> {
    Arc::new(config::TermConfig::with_config(
        config.adjusted(|config| config.enable_kitty_keyboard = true),
    ))
}

#[test]
fn normalized_plan_excludes_source_launch_commands() {
    let plan = into_plan(snapshot(), ImportMode::Layout);
    plan.validate().unwrap();
    let encoded = serde_json::to_string(&plan).unwrap();
    assert!(!encoded.contains("must-not"));
    assert_eq!(plan.projects[0].threads[0].active_tab, 1);
    assert_eq!(plan.pane_count(), 3);
    assert_eq!(plan.projects[0].threads[0].tabs[0].focused, Some(2));
    assert!(plan.projects[0].threads[0].tabs[0].zoomed);
}

#[test]
fn saved_adapter_discovers_previews_and_rechecks_changed_layout() {
    let directory = tempfile::tempdir().unwrap();
    let context = ImportContext {
        home: directory.path().to_owned(),
        config_home: None,
        executable: "/tmp/example".into(),
    };
    let root = context.home.join(".config/herdr");
    std::fs::create_dir_all(&root).unwrap();
    let mut saved = snapshot();
    for pane in saved
        .workspaces
        .iter_mut()
        .flat_map(|w| &mut w.tabs)
        .flat_map(|t| t.panes.values_mut())
    {
        pane.cwd = directory.path().to_str().unwrap().into();
    }
    std::fs::write(
        root.join("session.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    assert_eq!(HERDR.discover(&context).unwrap()[0].name, "default");
    let preview = HERDR.preview(&context, "default").unwrap();
    assert!(!preview.live);
    assert_eq!(preview.pane_count(), 3);
    let request = ImportRequest {
        source: "herdr".into(),
        session: "default".into(),
        mode: ImportMode::Layout,
        fingerprint: preview.fingerprint,
        space_name: "Imported".into(),
    };
    let prepared = HERDR.prepare(&context, &request).unwrap();
    prepared.validate().unwrap();
    assert_eq!(prepared.plan.pane_count(), 3);
    assert!(prepared.terminals.is_empty());
    saved.workspaces[0].custom_name = Some("changed".into());
    std::fs::write(
        root.join("session.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    assert!(HERDR.prepare(&context, &request).is_err());
}

#[test]
fn known_failures_are_localized_through_error_contexts() {
    for (reason, key) in [
        (
            "This running Herdr server does not support live handoff",
            "herdr-error-live-unsupported",
        ),
        (
            "A saved Herdr working directory is unavailable",
            "session-import-error-directory-missing",
        ),
        (
            "Herdr changed since the preview; inspect the session again",
            "session-import-error-changed",
        ),
    ] {
        let error = anyhow::anyhow!(reason).context("Import local session");
        assert_eq!(HERDR.error_key(&format!("{error:#}")), Some(key));
    }
}
