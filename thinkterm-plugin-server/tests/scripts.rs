//! Plugins that are a Python script, run by the Python found on this
//! machine: started, started again once edited, saying why they stopped,
//! and served again once mended. And what `thinkterm plugin new` leans
//! on: a plugin just put in place is turned on at once, and only from
//! where it was put.
//!
//! The tests that run a script need a Python 3.8 or later here, and say
//! so and pass without one.

mod common;

use common::{allow, host_in, next_notice, session, stop, wait_until, WAIT};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use thinkterm_plugin_channel::client::{Answer, Host, Notice, Session};
use thinkterm_plugin_channel::registry::{self as api, Info, State};
use thinkterm_plugin_channel::wire::{PageArgs, PanelEvent, PanelRequest, Raw};

fn call(session: &Session, plugin: &str, body: Value) -> Answer {
    let (tx, rx) = mpsc::channel();
    session.call(plugin, body, move |answer| {
        let _ = tx.send(answer);
    });
    rx.recv_timeout(WAIT).expect("every call is answered")
}

fn manage(session: &Session, request: api::Request) -> Answer {
    call(session, api::PLUGIN, serde_json::to_value(request).unwrap())
}

fn info(session: &Session, id: &str) -> Info {
    let listed = manage(
        session,
        api::Request::List {
            locale: String::new(),
        },
    );
    let listed: Vec<Info> = serde_json::from_value(listed.unwrap()).unwrap();
    listed
        .into_iter()
        .find(|info| info.id == id)
        .unwrap_or_else(|| panic!("{id} is not listed"))
}

fn logs(session: &Session, id: &str) -> api::Logs {
    let logs = manage(session, api::Request::Logs { id: id.into() }).unwrap();
    serde_json::from_value(logs).unwrap()
}

/// Whether a Python that script plugins run on is here.
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
        eprintln!("no Python 3.8 or later here: a script plugin is not tried");
    }
    here
}

const MANIFEST: &str = r#"
id = "script"
name = "Script"
version = "1"
api = 3

[run]
python = "main.py"
args = ["--from-manifest"]

[panel]
icon = "bug"

[page]
icon = "bug"
"#;

/// Answers `version` with its Python's, `env` with what it was given,
/// `boom` by raising, and anything else with its process id; draws "open"
/// when a panel opens.
const MAIN: &str = r#"import json, os, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

with open(os.path.join(os.environ["THINKTERM_PLUGIN_DATA"], "pid"), "w") as kept:
    kept.write(str(os.getpid()))
send({"type": "ready", "api": 3})
for line in sys.stdin:
    message = json.loads(line)
    kind = message.get("type")
    if kind == "stop":
        break
    if kind == "call":
        op = message["body"].get("op")
        if op == "boom":
            nope()
        if op == "version":
            body = list(sys.version_info[:2])
        elif op == "env":
            body = {
                "argv": sys.argv,
                "utf8": os.environ.get("PYTHONUTF8"),
                "bytecode": os.environ.get("PYTHONDONTWRITEBYTECODE"),
                "id": os.environ.get("THINKTERM_PLUGIN_ID"),
                "cwd": os.getcwd(),
            }
        elif op == "write":
            with open(os.path.join(os.getcwd(), "state.json"), "w") as state:
                state.write(json.dumps(message["body"]))
            body = os.getpid()
        else:
            body = os.getpid()
        send({"type": "ok", "id": message["id"], "body": body, "watch": op == "watch"})
    if kind == "open":
        text = {"x": 0, "y": 0, "w": 10, "h": 10, "text": "open"}
        send({"type": "frame", "view": message["view"], "frame": {"items": [{"text": text}]}})
"#;

/// Says why on standard error, and exits before it is ready.
const BROKEN: &str = "import sys\nraise SystemExit('cannot start: no config')\n";

fn install_script(host: &Host, main: &str) -> PathBuf {
    let dir = host.data_dir.join("plugins").join("script");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("main.py"), main).unwrap();
    allow(host, "script", &dir);
    dir
}

fn pid(answer: Answer) -> u64 {
    answer.unwrap().as_u64().expect("a process id")
}

#[test]
fn a_python_script_runs_on_the_python_found_here() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, MAIN);
    let (session, _notices) = session(&host);

    let version = call(&session, "script", json!({"op": "version"})).unwrap();
    let version = (version[0].as_u64().unwrap(), version[1].as_u64().unwrap());
    assert!(version >= (3, 8), "{version:?}");
    let given = call(&session, "script", json!({"op": "env"})).unwrap();
    // The script as it was found, by its whole path: a name starting with
    // "-" is no option.
    let script = std::fs::canonicalize(plugin.join("main.py")).unwrap();
    assert_eq!(
        given["argv"],
        json!([script.display().to_string(), "--from-manifest"])
    );
    assert_eq!(given["utf8"], "1");
    assert_eq!(given["bytecode"], "1", "no compiled files beside its own");
    assert_eq!(given["id"], "script");
    assert_eq!(
        PathBuf::from(given["cwd"].as_str().unwrap()),
        std::fs::canonicalize(&plugin).unwrap()
    );
    assert_eq!(info(&session, "script").state, State::Running);
    drop(session);
    stop(&host);
}

#[test]
fn a_script_is_started_again_once_any_of_its_files_changes() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, MAIN);
    let (session, _notices) = session(&host);
    let first = pid(call(&session, "script", json!({"op": "pid"})));
    assert_eq!(pid(call(&session, "script", json!({"op": "pid"}))), first);

    std::fs::write(plugin.join("main.py"), format!("{MAIN}# edited\n")).unwrap();
    let second = pid(call(&session, "script", json!({"op": "pid"})));
    assert_ne!(second, first, "the script was edited");

    // A file beside it, which it may load.
    std::fs::write(plugin.join("helper.py"), "x = 1\n").unwrap();
    let third = pid(call(&session, "script", json!({"op": "pid"})));
    assert_ne!(third, second, "a file was added beside it");

    // What a tool writes there is not the plugin.
    std::fs::create_dir_all(plugin.join("__pycache__")).unwrap();
    std::fs::write(plugin.join("__pycache__").join("main.pyc"), "x").unwrap();
    assert_eq!(pid(call(&session, "script", json!({"op": "pid"}))), third);
    // Nor what the plugin writes beside itself as it runs.
    assert_eq!(
        pid(call(&session, "script", json!({"op": "write", "n": 1}))),
        third
    );
    assert!(plugin.join("state.json").exists());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(pid(call(&session, "script", json!({"op": "pid"}))), third);
    drop(session);
    stop(&host);
}

#[test]
fn what_a_script_raised_is_why_it_stopped_and_is_in_its_logs() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_script(&host, MAIN);
    let (session, _notices) = session(&host);
    let line = MAIN
        .lines()
        .position(|line| line.contains("nope()"))
        .unwrap()
        + 1;
    let raised = format!("NameError: name 'nope' is not defined (main.py, line {line})");

    let why = call(&session, "script", json!({"op": "boom"})).unwrap_err();
    assert!(why.contains(&raised), "{why}");
    let state = info(&session, "script").state;
    assert!(
        matches!(&state, State::Crashed { reason } if reason.contains(&raised)),
        "{state:?}"
    );

    let logs = logs(&session, "script");
    assert!(
        logs.lines[0].starts_with("-- started at "),
        "{:?}",
        logs.lines
    );
    assert!(logs
        .lines
        .iter()
        .any(|line| line == "Traceback (most recent call last):"));
    assert!(
        logs.lines.last().unwrap().contains("NameError"),
        "{:?}",
        logs.lines
    );
    assert!(!logs.cut);
    assert!(manage(
        &session,
        api::Request::Logs {
            id: "nobody".into()
        }
    )
    .unwrap_err()
    .contains("no plugin"));
    drop(session);
    stop(&host);
}

#[test]
fn a_script_given_up_on_runs_again_once_mended() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, BROKEN);
    let (session, _notices) = session(&host);

    for _ in 0..3 {
        let why = call(&session, "script", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("cannot start: no config"), "{why}");
    }
    let state = info(&session, "script").state;
    assert!(
        matches!(&state, State::Failed { reason } if reason.contains("cannot start: no config")),
        "{state:?}"
    );
    // Given up on: refused at once, as it was.
    let why = call(&session, "script", json!({"op": "pid"})).unwrap_err();
    assert!(why.contains("3 times within a minute"), "{why}");

    std::fs::write(plugin.join("main.py"), MAIN).unwrap();
    pid(call(&session, "script", json!({"op": "pid"})));
    assert_eq!(info(&session, "script").state, State::Running);
    drop(session);
    stop(&host);
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

fn open(session: &Session, view: u64) {
    session.panel(
        view,
        PanelRequest::Open {
            plugin: "script".into(),
            env: env(),
            extends: None,
            page: None,
        },
    );
}

/// Opens the plugin's panel -- or a page, with `page` -- as `view`, from a
/// client that draws pictures of its views when asked.
fn open_drawing(session: &Session, view: u64, page: bool) {
    let mut env: Value = env().read().unwrap();
    env["features"] = json!(["picture"]);
    session.panel(
        view,
        PanelRequest::Open {
            plugin: "script".into(),
            env: Raw::new(&env),
            extends: None,
            page: page.then(|| PageArgs {
                args: json!({}),
                id: format!("p{view}"),
            }),
        },
    );
}

/// Asks the host for a picture of the plugin's view, as `thinkterm plugin
/// shot` does: answered once the client has drawn it.
fn ask_picture(session: &Session, page: Option<bool>) -> mpsc::Receiver<Answer> {
    let (tx, rx) = mpsc::channel();
    let request = api::Request::Picture {
        id: "script".into(),
        page,
    };
    session.call(
        api::PLUGIN,
        serde_json::to_value(request).unwrap(),
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    rx
}

/// The next thing heard about panel `view`, but that a press reached it.
fn heard(notices: &mpsc::Receiver<Notice>, view: u64) -> PanelEvent {
    loop {
        let notice = next_notice(
            notices,
            |notice| matches!(notice, Notice::Panel { view: of, .. } if *of == view),
        );
        match notice {
            Notice::Panel {
                event: PanelEvent::Heard,
                ..
            } => {}
            Notice::Panel { event, .. } => return event,
            _ => unreachable!(),
        }
    }
}

/// Opens a view with `open` until it is drawn, as a client does: opened
/// again after a pause whenever the host says it may work then -- the
/// plugin's Python being looked for, say.
fn opened(notices: &mpsc::Receiver<Notice>, view: u64, open: impl Fn()) -> Value {
    loop {
        open();
        match heard(notices, view) {
            PanelEvent::Frame { frame } => return frame.read().unwrap(),
            PanelEvent::Closed { again: true, .. } => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            event => panic!("not drawn: {event:?}"),
        }
    }
}

#[test]
fn a_view_refused_while_its_plugin_cannot_run_is_opened_again_once_it_can() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, BROKEN);
    let (session, notices) = session(&host);
    for _ in 0..3 {
        call(&session, "script", json!({"op": "pid"})).unwrap_err();
    }
    open(&session, 1);
    match heard(&notices, 1) {
        PanelEvent::Closed { reason, again } => {
            assert!(!again, "given up on");
            assert!(reason.contains("cannot start: no config"), "{reason}");
        }
        event => panic!("not closed: {event:?}"),
    }

    // Mended: the client still showing the view is told to open it again.
    std::fs::write(plugin.join("main.py"), MAIN).unwrap();
    match heard(&notices, 1) {
        PanelEvent::Closed { reason, again } => {
            assert!(again, "{reason}");
            assert_eq!(reason, "it can run now");
        }
        event => panic!("not told to open again: {event:?}"),
    }
    open(&session, 1);
    let PanelEvent::Frame { frame } = heard(&notices, 1) else {
        panic!("not drawn");
    };
    let frame: Value = frame.read().unwrap();
    assert_eq!(frame["items"][0]["text"]["text"], "open");
    drop(session);
    stop(&host);
}

#[test]
fn a_script_on_show_is_started_again_once_edited_with_nobody_asking() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, MAIN);
    let (session, notices) = session(&host);
    opened(&notices, 1, || open(&session, 1));
    session.panel(1, PanelRequest::Shown);

    // Edited as an editor writes: twice in a row. The panel's view closes
    // for its client to open again, once.
    let edited = MAIN.replace("\"text\": \"open\"", "\"text\": \"edited\"");
    std::fs::write(
        plugin.join("main.py"),
        MAIN.replace("os.getpid()", "os.getpid() "),
    )
    .unwrap();
    std::fs::write(plugin.join("main.py"), &edited).unwrap();
    match heard(&notices, 1) {
        PanelEvent::Closed { again, reason } => assert!(again, "{reason}"),
        event => panic!("not closed to open again: {event:?}"),
    }
    open(&session, 1);
    let drawn = loop {
        match heard(&notices, 1) {
            PanelEvent::Frame { frame } => break frame.read::<Value>().unwrap(),
            // Opened while the old run was still on its way out.
            PanelEvent::Closed { again: true, .. } => open(&session, 1),
            event => panic!("not drawn: {event:?}"),
        }
    };
    assert_eq!(drawn["items"][0]["text"]["text"], "edited");
    drop(session);
    stop(&host);
}

/// A plugin served by the host's own program, as `id`: no Python needed.
fn served(host: &Host, id: &str) -> String {
    format!(
        "id = \"{id}\"\nname = \"Served\"\nversion = \"1\"\napi = 1\n\n[run]\nprogram = '{}'\n\
         args = [\"--serve-plugin\", \"snippets\", \"--data-dir\", '{}']\n",
        host.program.display(),
        host.data_dir.join(format!("served-{id}")).display()
    )
}

fn put(host: &Host, name: &str, manifest: &str) -> PathBuf {
    let dir = host.data_dir.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
    dir
}

#[test]
fn a_plugin_put_in_place_while_the_host_runs_is_turned_on_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (session, _notices) = session(&host);
    // The host is up, and has looked at its plugins directory already.
    info(&session, "snippets");

    put(&host, "fresh", &served(&host, "fresh"));
    manage(
        &session,
        api::Request::SetEnabled {
            id: "fresh".into(),
            enabled: true,
        },
    )
    .unwrap();
    assert_eq!(
        call(&session, "fresh", json!({"op": "list", "query": ""})),
        Ok(json!([]))
    );
    drop(session);
    stop(&host);
}

fn allow_at(session: &Session, id: &str, dir: &Path) -> Answer {
    manage(
        session,
        api::Request::Allow {
            id: id.into(),
            dir: dir.display().to_string(),
        },
    )
}

#[test]
fn allowing_turns_on_only_the_plugin_in_that_directory() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (session, _notices) = session(&host);
    // Two claim one id: the one whose directory sorts first answers to it.
    let first = put(&host, "a-dup", &served(&host, "dup"));
    let second = put(&host, "b-dup", &served(&host, "dup"));

    let why = allow_at(&session, "dup", &second).unwrap_err();
    assert!(why.contains("is not used"), "{why}");
    let why = allow_at(&session, "other", &first).unwrap_err();
    assert!(why.contains("is \"dup\", not \"other\""), "{why}");
    let why = allow_at(&session, "dup", &dir.path().join("elsewhere")).unwrap_err();
    assert!(why.contains("not a plugin's directory"), "{why}");
    assert_eq!(
        info(&session, "dup").state,
        State::New,
        "none of that let it run"
    );

    allow_at(&session, "dup", &first).unwrap();
    assert!(info(&session, "dup").enabled);
    assert_eq!(
        call(&session, "dup", json!({"op": "list", "query": ""})),
        Ok(json!([]))
    );
    drop(session);
    stop(&host);
}

#[test]
fn a_picture_of_a_view_is_drawn_by_its_client_and_kept_for_the_user() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install_script(&host, MAIN);
    let (session, notices) = session(&host);

    let why = ask_picture(&session, None)
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap_err();
    assert!(why.contains("no view of \"script\" is open"), "{why}");

    // A panel from a client that cannot draw one.
    opened(&notices, 1, || open(&session, 1));
    let why = ask_picture(&session, None)
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap_err();
    assert!(why.contains("cannot be drawn into a picture"), "{why}");

    // And a page from one that can: the one view that can be drawn is.
    opened(&notices, 2, || open_drawing(&session, 2, true));
    let answer = ask_picture(&session, None);
    let id = match heard(&notices, 2) {
        PanelEvent::Picture { id } => id,
        event => panic!("not asked for a picture: {event:?}"),
    };
    let png = b"\x89PNG\r\n\x1a\n not really".to_vec();
    let picture = json!({"result": "drawn", "png": base64_of(&png), "width": 600, "height": 1000});
    session.panel(
        2,
        PanelRequest::Picture {
            id,
            picture: Raw::new(&picture),
        },
    );
    let kept: api::Picture =
        serde_json::from_value(answer.recv_timeout(WAIT).unwrap().unwrap()).unwrap();
    assert!(kept.page);
    assert_eq!((kept.width, kept.height), (600, 1000));
    assert_eq!(std::fs::read(&kept.path).unwrap(), png);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&kept.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the user's alone: {mode:o}");
    }

    // Two that can be drawn: which is to be said.
    opened(&notices, 3, || open_drawing(&session, 3, false));
    let why = ask_picture(&session, None)
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap_err();
    assert!(
        why.contains("its panel and a page") && why.contains("--page or --panel"),
        "{why}"
    );
    let answer = ask_picture(&session, Some(false));
    let id = match heard(&notices, 3) {
        PanelEvent::Picture { id } => id,
        event => panic!("the panel not asked: {event:?}"),
    };
    let failed = json!({"result": "failed", "why": "no fonts"});
    session.panel(
        3,
        PanelRequest::Picture {
            id,
            picture: Raw::new(&failed),
        },
    );
    let why = answer.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(why.contains("could not draw it: no fonts"), "{why}");

    // A view that closes before it is drawn answers the call too.
    let answer = ask_picture(&session, Some(true));
    let PanelEvent::Picture { .. } = heard(&notices, 2) else {
        panic!("the page not asked")
    };
    session.panel(2, PanelRequest::Close);
    let why = answer.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(why.contains("closed before it was drawn"), "{why}");
    drop(session);
    stop(&host);
}

fn base64_of(bytes: &[u8]) -> String {
    serde_json::to_value(thinkterm_plugin_sdk::panel::Bytes(bytes.to_vec()))
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_script_only_watched_is_started_again_once_edited() {
    if !python_here() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let plugin = install_script(&host, MAIN);
    let (session, _notices) = session(&host);
    let kept = host.data_dir.join("plugin-data").join("script").join("pid");
    // The session watches it: no view, no call waiting, only that.
    let first = pid(call(&session, "script", json!({"op": "watch"})));
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), first.to_string());

    std::fs::write(plugin.join("main.py"), format!("{MAIN}# edited\n")).unwrap();
    wait_until("it to start again by itself", || {
        std::fs::read_to_string(&kept).is_ok_and(|pid| pid != first.to_string())
    });
    drop(session);
    stop(&host);
}
