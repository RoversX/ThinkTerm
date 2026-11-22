//! The plugin `thinkterm plugin new` makes, plugins/python-template, run
//! through the host as it is shipped: its Python helper speaks the
//! protocol, draws its widgets, and hears what the user does in them.
//!
//! Needs a Python 3.8 or later here; says so and passes without one.

mod common;

use common::{allow, host_in, next_notice, session, stop, WAIT};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use thinkterm_plugin_channel::client::{Answer, Host, Notice, Session};
use thinkterm_plugin_channel::wire::{PageArgs, PanelEvent, PanelRequest, Raw};

fn template() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("plugins")
        .join("python-template")
}

fn python_here() -> bool {
    let here = ["python3", "python"].iter().any(|name| {
        std::process::Command::new(name)
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)",
            ])
            .status()
            .is_ok_and(|status| status.success())
    });
    if !here {
        eprintln!("no Python 3.8 or later here: the template is not tried");
    }
    here
}

/// The template installed as it ships, its main.py changed by `edit`.
fn install(host: &Host, edit: impl Fn(String) -> String) -> PathBuf {
    let dir = host.data_dir.join("plugins").join("hello-python");
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["plugin.toml", "thinkterm.py"] {
        std::fs::copy(template().join(file), dir.join(file)).unwrap();
    }
    let main = std::fs::read_to_string(template().join("main.py")).unwrap();
    std::fs::write(dir.join("main.py"), edit(main)).unwrap();
    allow(host, "hello-python", &dir);
    dir
}

fn call(session: &Session, body: Value) -> Answer {
    let (tx, rx) = mpsc::channel();
    session.call("hello-python", body, move |answer| {
        let _ = tx.send(answer);
    });
    rx.recv_timeout(WAIT).expect("every call is answered")
}

fn env() -> Raw {
    Raw::new(&json!({
        "width": 320, "height": 600, "scale": 2, "dark": true,
        "small": {"size": 11, "line": 15},
        "body": {"size": 13, "line": 18},
        "title": {"size": 15, "line": 20},
        "mono": {"size": 12, "line": 17, "advance": 7},
        "features": ["fields", "keys", "wrap"]
    }))
}

fn open(session: &Session, view: u64, page: Option<PageArgs>) {
    session.panel(
        view,
        PanelRequest::Open {
            plugin: "hello-python".into(),
            env: env(),
            extends: None,
            page,
        },
    );
}

fn input(session: &Session, view: u64, input: Value) {
    session.panel(
        view,
        PanelRequest::Input {
            input: Raw::new(&input),
        },
    );
}

/// The next frame drawn for `view`, and that taken in.
fn frame(session: &Session, notices: &mpsc::Receiver<Notice>, view: u64) -> Value {
    loop {
        let notice = next_notice(
            notices,
            |notice| matches!(notice, Notice::Panel { view: of, .. } if *of == view),
        );
        match notice {
            Notice::Panel {
                event: PanelEvent::Frame { frame },
                ..
            } => {
                session.panel(view, PanelRequest::Shown);
                return frame.read().unwrap();
            }
            Notice::Panel {
                event: PanelEvent::Heard,
                ..
            } => {}
            Notice::Panel { event, .. } => panic!("not a frame: {event:?}"),
            _ => unreachable!(),
        }
    }
}

/// Opens view `view` -- a page with `page` -- until it is drawn, as a
/// client does: again after a pause whenever the host says it may work then,
/// its plugin's Python being looked for, say. Its first frame.
fn opened(
    session: &Session,
    notices: &mpsc::Receiver<Notice>,
    view: u64,
    page: Option<PageArgs>,
) -> Value {
    loop {
        open(session, view, page.clone());
        let notice = next_notice(
            notices,
            |notice| matches!(notice, Notice::Panel { view: of, event } if *of == view && !matches!(event, PanelEvent::Heard)),
        );
        match notice {
            Notice::Panel {
                event: PanelEvent::Frame { frame },
                ..
            } => {
                session.panel(view, PanelRequest::Shown);
                return frame.read().unwrap();
            }
            Notice::Panel {
                event: PanelEvent::Closed { again: true, .. },
                ..
            } => std::thread::sleep(std::time::Duration::from_millis(100)),
            Notice::Panel { event, .. } => panic!("not drawn: {event:?}"),
            _ => unreachable!(),
        }
    }
}

/// What a frame's items of `kind` say under `key`.
fn said(frame: &Value, kind: &str, key: &str) -> Vec<String> {
    frame["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get(kind)?.get(key)?.as_str().map(str::to_string))
        .collect()
}

fn texts(frame: &Value) -> Vec<String> {
    said(frame, "text", "text")
}

#[test]
fn the_template_draws_its_panel_and_hears_clicks_and_typing() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, |main| main);
    let (session, notices) = session(&host);

    assert_eq!(
        call(&session, json!({})),
        Ok(json!({"clicks": 0, "name": ""}))
    );
    let drawn = opened(&session, &notices, 1, None);
    let shown = texts(&drawn);
    for expected in [
        "Hello",
        "Click me",
        "Reset",
        "Clicked 0 times",
        "Count by two",
    ] {
        assert!(
            shown.iter().any(|text| text == expected),
            "{expected} in {shown:?}"
        );
    }
    assert!(
        !shown.iter().any(|text| text.contains("plugin's page")),
        "a panel"
    );
    let hits = said(&drawn, "hit", "id");
    assert!(hits.contains(&"button:Click me".to_string()), "{hits:?}");
    assert_eq!(said(&drawn, "field", "id"), ["field:Your name"]);

    input(
        &session,
        1,
        json!({"click": {"id": "button:Click me", "x": 1, "y": 1}}),
    );
    assert!(texts(&frame(&session, &notices, 1)).contains(&"Clicked 1 times".to_string()));
    input(
        &session,
        1,
        json!({"click": {"id": "toggle:Count by two", "x": 1, "y": 1}}),
    );
    frame(&session, &notices, 1);
    input(
        &session,
        1,
        json!({"click": {"id": "button:Click me", "x": 1, "y": 1}}),
    );
    assert!(texts(&frame(&session, &notices, 1)).contains(&"Clicked 3 times".to_string()));

    input(
        &session,
        1,
        json!({"text": {"id": "field:Your name", "text": "Ada 李"}}),
    );
    let typed = frame(&session, &notices, 1);
    assert!(
        texts(&typed).contains(&"Hello, Ada 李!".to_string()),
        "{:?}",
        texts(&typed)
    );
    input(
        &session,
        1,
        json!({"submit": {"id": "field:Your name", "text": "Ada 李"}}),
    );
    assert!(texts(&frame(&session, &notices, 1)).contains(&"Clicked 13 times".to_string()));
    assert_eq!(
        call(&session, json!({})),
        Ok(json!({"clicks": 13, "name": "Ada 李"}))
    );

    // A call that changes what is shown draws it again.
    call(&session, json!({"op": "reset"})).unwrap();
    assert!(texts(&frame(&session, &notices, 1)).contains(&"Clicked 0 times".to_string()));

    // Its page, drawn by the same draw().
    let page = texts(&opened(
        &session,
        &notices,
        2,
        Some(PageArgs {
            args: json!({}),
            id: "p1".into(),
        }),
    ));
    assert!(
        page.iter().any(|text| text.contains("plugin's page")),
        "{page:?}"
    );
    drop(session);
    stop(&host);
}

#[test]
fn what_draw_raises_is_shown_in_the_view_and_logged() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, |main| {
        main.replace("ui.title(\"Hello\")", "ui.title(\"Hello\" + missing)")
    });
    let (session, notices) = session(&host);
    let shown = texts(&opened(&session, &notices, 1, None));
    assert_eq!(shown[0], "draw() failed");
    assert!(
        shown[1].starts_with("NameError: name 'missing' is not defined"),
        "{shown:?}"
    );
    assert!(shown[2].starts_with("main.py, line "), "{shown:?}");
    // It goes on running, and says so on standard error once.
    let logs = loop {
        let logs = call_host(&session, json!({"op": "logs", "id": "hello-python"}));
        let lines: Vec<String> = serde_json::from_value(logs["lines"].clone()).unwrap();
        if lines.iter().any(|line| line.contains("NameError")) {
            break lines;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(logs
        .iter()
        .any(|line| line == "Traceback (most recent call last):"));
    assert_eq!(
        call(&session, json!({})),
        Ok(json!({"clicks": 0, "name": ""}))
    );
    drop(session);
    stop(&host);
}

fn call_host(session: &Session, body: Value) -> Value {
    let (tx, rx) = mpsc::channel();
    session.call("plugins", body, move |answer| {
        let _ = tx.send(answer);
    });
    rx.recv_timeout(WAIT).unwrap().unwrap()
}

#[test]
fn the_template_names_what_plugin_new_writes_over() {
    // `thinkterm plugin new` gives the template its id, name and the title
    // of its action by these lines: they are there as it expects.
    let manifest = std::fs::read_to_string(template().join("plugin.toml")).unwrap();
    for line in [
        "id = \"hello-python\"",
        "name = \"Hello Python\"",
        "title = \"Open Hello Python\"",
        "python = \"main.py\"",
    ] {
        assert_eq!(
            manifest.lines().filter(|have| *have == line).count(),
            1,
            "{line}"
        );
    }
}
