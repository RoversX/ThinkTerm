//! Plugins' panels as clients see them: opened, drawn, clicked and scrolled
//! through the host, sent one frame at a time, asking things of another
//! machine through the client, and closed when the plugin or the client
//! goes.

#![cfg(unix)]

mod common;

use common::{host_in, next_notice, session, stop, wait_until};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use thinkterm_plugin_channel::client::{Host, Notice, Session};
use thinkterm_plugin_channel::registry as api;
use thinkterm_plugin_channel::wire::{PanelEvent, PanelRequest, Raw};

/// Draws "open" when a panel opens -- "extends" and the panel's number for
/// an extended view -- or what is in the file `huge` in its data directory
/// when there is one, and then, when there is a file `ask` there, asks what
/// is at /x, by the view's number. A click draws five frames, one after
/// another, numbered; rows are answered with one row, and then with one
/// nobody asked for. Every view closed, and every answer, is written down.
const PANEL: &str = r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":2}'
frame() {
  printf '{"type":"frame","view":%s,"frame":{"items":[{"text":{"x":0,"y":0,"w":10,"h":10,"text":"%s"}}]}}\n' "$1" "$2"
}
while IFS= read -r line; do
  view=$(printf '%s' "$line" | sed -n 's/.*"view":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) exit 0 ;;
    *'"type":"open"'*)
      extends=$(printf '%s' "$line" | sed -n 's/.*"extends":\([0-9]*\).*/\1/p')
      if [ -f "$THINKTERM_PLUGIN_DATA/huge" ]; then cat "$THINKTERM_PLUGIN_DATA/huge"
      elif [ -n "$extends" ]; then frame "$view" "extends $extends"
      else frame "$view" open; fi
      if [ -f "$THINKTERM_PLUGIN_DATA/ask" ]; then
        printf '{"type":"ask","view":%s,"id":%s,"machine":"m1","ask":{"op":"stat","path":"/x"}}\n' "$view" "$view"
      fi ;;
    *'"type":"answer"'*) printf '%s\n' "$line" >> "$THINKTERM_PLUGIN_DATA/answers" ;;
    *'"type":"input"'*) for n in 1 2 3 4 5; do frame "$view" "$n"; done ;;
    *'"type":"rows"'*)
      printf '{"type":"rows","view":%s,"rows":{"list":"l","key":"k","from":0,"rows":[[]]}}\n' "$view"
      printf '{"type":"rows","view":%s,"rows":{"list":"l","key":"k","from":9,"rows":[[]]}}\n' "$view" ;;
    *'"type":"close"'*) printf '%s\n' "$view" >> "$THINKTERM_PLUGIN_DATA/closed" ;;
  esac
done
"#;

const MANIFEST: &str = r#"
id = "panel"
name = "Panel"
version = "1"
api = 2

[run]
program = "plugin.sh"

[panel]
icon = "chart-line"
"#;

fn install_panel(host: &Host) -> PathBuf {
    let dir = host.data_dir.join("plugins").join("panel");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), MANIFEST).unwrap();
    let program = dir.join("plugin.sh");
    std::fs::write(&program, PANEL).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn env() -> Raw {
    Raw::new(&json!({
        "width": 300, "height": 500, "scale": 2, "dark": true,
        "small": {"size": 11, "line": 15},
        "body": {"size": 13, "line": 18},
        "title": {"size": 15, "line": 20},
        "mono": {"size": 12, "line": 17, "advance": 7}
    }))
}

/// [`env`], beside a terminal on another machine.
fn remote_env() -> Raw {
    let mut env: Value = env().read().unwrap();
    env["remote"] = json!({"host": "server-a", "machine": "m1", "cwd": "/home/user"});
    Raw::new(&env)
}

fn open(session: &Session, view: u64) {
    session.panel(
        view,
        PanelRequest::Open {
            plugin: "panel".into(),
            env: env(),
            extends: None,
        },
    );
}

/// Opens the extended view of the client's panel `panel` as `view`.
fn extend(session: &Session, view: u64, panel: u64) {
    session.panel(
        view,
        PanelRequest::Open {
            plugin: "panel".into(),
            env: env(),
            extends: Some(panel),
        },
    );
}

fn click(session: &Session, view: u64) {
    let input = Raw::new(&json!({"click": {"id": "a", "x": 1, "y": 1}}));
    session.panel(view, PanelRequest::Input { input });
}

/// The next thing heard about panel `view`.
fn heard(notices: &mpsc::Receiver<Notice>, view: u64) -> PanelEvent {
    let notice = next_notice(
        notices,
        |notice| matches!(notice, Notice::Panel { view: of, .. } if *of == view),
    );
    let Notice::Panel { event, .. } = notice else {
        unreachable!()
    };
    event
}

/// What a frame says: the text it draws.
fn drawn(event: PanelEvent) -> String {
    let PanelEvent::Frame { frame } = &event else {
        panic!("not a frame: {event:?}")
    };
    let frame: Value = frame.read().unwrap();
    frame["items"][0]["text"]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Nothing more is heard about panel `view` for a while.
fn quiet(notices: &mpsc::Receiver<Notice>, view: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match notices.recv_timeout(left) {
            Ok(Notice::Panel { view: of, event }) if of == view => {
                panic!("heard {event:?}")
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn closed_views(host: &Host) -> String {
    std::fs::read_to_string(host.data_dir.join("plugin-data/panel/closed")).unwrap_or_default()
}

#[test]
fn a_panel_is_listed_opened_drawn_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let (session, notices) = session(&host);

    let listed: Vec<api::Info> = {
        let (tx, rx) = mpsc::channel();
        let request = serde_json::to_value(api::Request::List {
            locale: String::new(),
        })
        .unwrap();
        session.call(api::PLUGIN, request, move |answer| {
            let _ = tx.send(answer);
        });
        serde_json::from_value(rx.recv().unwrap().unwrap()).unwrap()
    };
    let panel = listed.iter().find(|info| info.id == "panel").unwrap();
    assert_eq!(panel.panel.as_ref().unwrap().icon, "chart-line");
    assert!(listed
        .iter()
        .find(|info| info.id == "snippets")
        .unwrap()
        .panel
        .is_none());

    open(&session, 41);
    assert_eq!(drawn(heard(&notices, 41)), "open");
    session.panel(41, PanelRequest::Shown);

    let wanted = Raw::new(&json!({"list": "l", "key": "k", "from": 0, "to": 1}));
    session.panel(41, PanelRequest::Rows { wanted });
    let PanelEvent::Rows { rows } = heard(&notices, 41) else {
        panic!()
    };
    assert_eq!(rows.read::<Value>().unwrap()["from"], 0);
    quiet(&notices, 41);

    session.panel(41, PanelRequest::Close);
    wait_until("the plugin hears the panel closed", || {
        closed_views(&host) == "1\n"
    });
    drop(session);
    stop(&host);
}

#[test]
fn an_extended_view_knows_its_panel_and_closes_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let (session, notices) = session(&host);
    open(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "open");
    extend(&session, 2, 1);
    // The plugin knows views by its own numbers: the panel is its first.
    assert_eq!(drawn(heard(&notices, 2)), "extends 1");

    // Nothing extends a panel the client does not show, or an extended
    // view.
    for (view, panel) in [(3, 9), (4, 2)] {
        extend(&session, view, panel);
        let PanelEvent::Closed { again, reason } = heard(&notices, view) else {
            panic!()
        };
        assert!(!again && reason.contains("to extend"), "{reason}");
    }

    // The panel opened anew under its number: its extended view closed,
    // and the client is told.
    open(&session, 1);
    let PanelEvent::Closed { again, .. } = heard(&notices, 2) else {
        panic!()
    };
    assert!(!again);
    assert_eq!(drawn(heard(&notices, 1)), "open");
    extend(&session, 5, 1);
    assert_eq!(drawn(heard(&notices, 5)), "extends 3");
    // A panel has one: another takes its place.
    extend(&session, 6, 1);
    let PanelEvent::Closed { again, .. } = heard(&notices, 5) else {
        panic!()
    };
    assert!(!again);
    assert_eq!(drawn(heard(&notices, 6)), "extends 3");

    session.panel(1, PanelRequest::Close);
    let PanelEvent::Closed { .. } = heard(&notices, 6) else {
        panic!()
    };
    wait_until("the plugin hears each extended view close first", || {
        closed_views(&host) == "2\n1\n4\n5\n3\n"
    });
    drop(session);
    stop(&host);
}

#[test]
fn a_client_is_sent_the_newest_frame_once_it_took_in_the_last() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let (session, notices) = session(&host);
    open(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "open");
    // Five frames drawn while "open" is not taken in yet: none is sent.
    click(&session, 1);
    quiet(&notices, 1);
    session.panel(1, PanelRequest::Shown);
    assert_eq!(drawn(heard(&notices, 1)), "5", "only the newest");
    session.panel(1, PanelRequest::Shown);
    quiet(&notices, 1);
    drop(session);
    stop(&host);
}

#[test]
fn a_frame_too_long_to_send_is_dropped_and_the_client_keeps_its_connection() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    // A line as long as a plugin may send: the view's number around it
    // makes it longer than a client is sent.
    let data = host.data_dir.join("plugin-data/panel");
    std::fs::create_dir_all(&data).unwrap();
    let start = r#"{"type":"frame","view":1,"frame":{"items":[],"pad":""#;
    let end = r#""}}"#;
    let pad = thinkterm_plugin_sdk::protocol::MAX_LINE - start.len() - end.len();
    let line = format!("{start}{}{end}\n", "x".repeat(pad));
    std::fs::write(data.join("huge"), line).unwrap();
    let (session, notices) = session(&host);
    open(&session, 1);
    quiet(&notices, 1);
    // Nothing was sent, so nothing waits to be taken in: the next frame
    // goes at once, over the same connection.
    click(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "1");
    let log = std::fs::read_to_string(&host.log).unwrap();
    assert!(log.contains("too long to send"), "{log}");
    drop(session);
    stop(&host);
}

#[test]
fn a_panel_closes_with_its_plugin_and_opens_again_unless_it_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let (session, notices) = session(&host);
    let manage = |request: api::Request| {
        let (tx, rx) = mpsc::channel();
        session.call(
            api::PLUGIN,
            serde_json::to_value(request).unwrap(),
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        rx.recv().unwrap()
    };
    open(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "open");

    manage(api::Request::Reload {
        id: Some("panel".into()),
    })
    .unwrap();
    let PanelEvent::Closed { again, .. } = heard(&notices, 1) else {
        panic!()
    };
    assert!(again, "a reload is worth opening again after");
    open(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "open", "on the new run");
    // The old run's view was closed before it went; the new one opened
    // as view 2.
    session.panel(1, PanelRequest::Shown);

    manage(api::Request::SetEnabled {
        id: "panel".into(),
        enabled: false,
    })
    .unwrap();
    let PanelEvent::Closed { again, .. } = heard(&notices, 1) else {
        panic!()
    };
    assert!(again);
    open(&session, 1);
    let PanelEvent::Closed { reason, again } = heard(&notices, 1) else {
        panic!()
    };
    assert!(!again, "off, it stays closed: {reason}");
    assert!(reason.contains("turned off"), "{reason}");
    drop(session);
    stop(&host);
}

#[test]
fn a_client_that_leaves_takes_its_panels_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let (session, notices) = session(&host);
    open(&session, 1);
    open(&session, 2);
    assert_eq!(drawn(heard(&notices, 1)), "open");
    assert_eq!(drawn(heard(&notices, 2)), "open");
    // Another client keeps the host, and the plugin, up.
    let (other, other_notices) = common::session(&host);
    open(&other, 1);
    assert_eq!(drawn(heard(&other_notices, 1)), "open");

    drop(session);
    wait_until("both of its panels close", || {
        let closed = closed_views(&host);
        closed.lines().count() == 2 && closed.contains("1\n") && closed.contains("2\n")
    });
    drop(other);
    stop(&host);
}

#[test]
fn a_panel_asked_for_before_the_host_was_reached_is_opened_once_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let real = host_in(dir.path());
    install_panel(&real);
    // A session whose host cannot be started: what it is asked about a
    // panel meanwhile goes nowhere.
    let unreachable = Host {
        program: dir.path().join("no-such-program"),
        ..real.clone()
    };
    let (tx, notices) = mpsc::channel();
    let session = Session::start(unreachable, move |notice| {
        let _ = tx.send(notice);
    })
    .unwrap();
    next_notice(&notices, |notice| matches!(notice, Notice::Trouble(_)));
    open(&session, 1);
    // The host comes up after all: the owner is told to open its panels
    // again, though it never had a connection to lose them with.
    let mut server = common::spawn(&real, &[]);
    let connected = next_notice(&notices, |notice| {
        matches!(notice, Notice::Connected { .. })
    });
    assert!(
        matches!(connected, Notice::Connected { again: true }),
        "{connected:?}"
    );
    open(&session, 1);
    assert_eq!(drawn(heard(&notices, 1)), "open");
    drop(session);
    stop(&real);
    let _ = server.wait();
}

#[test]
fn a_panel_of_a_plugin_without_one_is_closed_for_good() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (session, notices) = session(&host);
    session.panel(
        3,
        PanelRequest::Open {
            plugin: "snippets".into(),
            env: env(),
            extends: None,
        },
    );
    let PanelEvent::Closed { again, reason } = heard(&notices, 3) else {
        panic!()
    };
    assert!(!again, "{reason}");
    session.panel(
        4,
        PanelRequest::Open {
            plugin: "panel".into(),
            env: Raw::new(&json!({"width": "wide"})),
            extends: None,
        },
    );
    let PanelEvent::Closed { again, reason } = heard(&notices, 4) else {
        panic!()
    };
    assert!(!again && reason.contains("size"), "{reason}");
    drop(session);
    stop(&host);
}

#[test]
fn what_a_plugin_asks_of_another_machine_goes_to_its_panels_client_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_panel(&host);
    let data = host.data_dir.join("plugin-data/panel");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("ask"), "").unwrap();
    let answers = || std::fs::read_to_string(data.join("answers")).unwrap_or_default();
    let (session, notices) = session(&host);
    let open_remote = |view| {
        session.panel(
            view,
            PanelRequest::Open {
                plugin: "panel".into(),
                env: remote_env(),
                extends: None,
            },
        )
    };

    // Beside a terminal on another machine: the ask reaches the client
    // showing the panel, and its answer the plugin.
    open_remote(7);
    assert_eq!(drawn(heard(&notices, 7)), "open");
    let PanelEvent::Remote { id, machine, ask } = heard(&notices, 7) else {
        panic!("no ask")
    };
    assert_eq!(machine, "m1", "the machine the plugin named");
    assert_eq!(
        ask.read::<Value>().unwrap(),
        json!({"op": "stat", "path": "/x"})
    );
    let answer = Raw::new(&json!({"result": "stat", "entry": null}));
    session.panel(7, PanelRequest::Answer { id, answer });
    wait_until("the plugin hears the answer", || {
        answers().contains(r#""result":"stat""#)
    });
    // An answer to nothing it asked goes nowhere.
    let stray = Raw::new(&json!({"result": "stat", "entry": null}));
    session.panel(
        7,
        PanelRequest::Answer {
            id: 99,
            answer: stray,
        },
    );

    // Beside one on this machine, the plugin reaches it itself: refused.
    open(&session, 8);
    assert_eq!(drawn(heard(&notices, 8)), "open");
    wait_until("the refusal", || answers().contains("runs on this machine"));
    quiet(&notices, 8);

    // Closed before the client answered: the plugin hears none is coming.
    open_remote(9);
    assert_eq!(drawn(heard(&notices, 9)), "open");
    let PanelEvent::Remote { .. } = heard(&notices, 9) else {
        panic!("no ask")
    };
    session.panel(9, PanelRequest::Close);
    wait_until("the plugin hears it goes unanswered", || {
        answers().contains("the panel closed")
    });
    assert_eq!(answers().lines().count(), 3, "{}", answers());
    drop(session);
    stop(&host);
}
