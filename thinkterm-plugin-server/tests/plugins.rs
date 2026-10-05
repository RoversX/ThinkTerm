//! Installed plugins as clients see them: found in the plugins directory,
//! started as programs of their own, turned off and on, reloaded, and given
//! up on when they misbehave.

mod common;

use common::{allow, host_in, next_notice, session, spawn, spawn_with_env, stop, wait_until, WAIT};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use thinkterm_plugin_channel::client::{Answer, Host, Notice, Session};
use thinkterm_plugin_channel::registry::{self as api, Info, State};
use thinkterm_plugin_channel::wire::{FromHost, ToHost};

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

fn list(session: &Session) -> Vec<Info> {
    let listed = manage(
        session,
        api::Request::List {
            locale: "zh-CN".into(),
        },
    );
    serde_json::from_value(listed.unwrap()).unwrap()
}

fn info(session: &Session, id: &str) -> Info {
    list(session)
        .into_iter()
        .find(|info| info.id == id)
        .unwrap_or_else(|| panic!("{id} is not listed"))
}

fn ids(listed: &[Info]) -> Vec<&str> {
    listed.iter().map(|info| info.id.as_str()).collect()
}

fn set_enabled(session: &Session, id: &str, enabled: bool) -> Answer {
    manage(
        session,
        api::Request::SetEnabled {
            id: id.into(),
            enabled,
        },
    )
}

/// Installs a plugin, and lets it run, as the user turning it on does: a
/// directory `name` in the host's plugins directory, holding `manifest`.
fn install(host: &Host, name: &str, manifest: &str) -> PathBuf {
    let dir = place(host, name, manifest);
    if let Some(id) = id_of(manifest) {
        allow(host, &id, &dir);
    }
    dir
}

/// Installs a plugin the user has not let run yet.
fn place(host: &Host, name: &str, manifest: &str) -> PathBuf {
    let dir = host.data_dir.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
    dir
}

/// The id a manifest gives, when it reads.
fn id_of(manifest: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(manifest).ok()?;
    Some(value.get("id")?.as_str()?.to_string())
}

/// Snippets installed again under another id, as a program of its own:
/// the host's own program serving it over standard input and output.
fn snippets_copy(host: &Host) -> String {
    format!(
        r#"
id = "snippets-copy"
name = "Snippets, out of process"
version = "1.0"
api = 1

[run]
program = '{}'
args = ["--serve-plugin", "snippets", "--data-dir", '{}']

[locales.zh-CN]
name = "片段副本"
"#,
        host.program.display(),
        host.data_dir.join("copy").display()
    )
}

fn is_event_of(plugin: &str) -> impl Fn(&Notice) -> bool + '_ {
    move |notice| matches!(notice, Notice::Event { plugin: from, .. } if from == plugin)
}

#[test]
fn an_installed_plugin_runs_as_a_program_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, "copy", &snippets_copy(&host));
    let (session, notices) = session(&host);

    let listed = list(&session);
    assert_eq!(ids(&listed), ["snippets", "snippets-copy"]);
    assert!(listed[0].builtin);
    assert_eq!(listed[0].name, "片段", "in the language asked for");
    assert_eq!(listed[1].name, "片段副本");
    assert_eq!(listed[1].version, "1.0");
    assert_eq!(listed[1].state, State::Idle, "nothing started it yet");

    let rows = call(
        &session,
        "snippets-copy",
        json!({"op": "list", "query": ""}),
    );
    assert_eq!(rows, Ok(json!([])));
    assert_eq!(info(&session, "snippets-copy").state, State::Running);
    let saved = call(
        &session,
        "snippets-copy",
        json!({"op": "save", "title": "", "body": "make"}),
    )
    .unwrap();
    assert_eq!(saved["outcome"], "saved");
    // The list made this session a watcher, and the save tells it.
    let Notice::Event { body, .. } = next_notice(&notices, is_event_of("snippets-copy")) else {
        unreachable!()
    };
    assert_eq!(body, json!({"event": "changed"}));
    // Its files are where it was told to keep them, apart from the
    // built-in plugin's.
    assert!(host.data_dir.join("copy/snippets.json").exists());
    assert!(!host.data_dir.join("snippets.json").exists());
    drop(session);
    stop(&host);
}

#[test]
fn a_plugin_turned_off_stops_and_refuses_calls_until_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, "copy", &snippets_copy(&host));
    let (session, _notices) = session(&host);
    let query = json!({"op": "list", "query": ""});
    call(&session, "snippets-copy", query.clone()).unwrap();

    assert_eq!(
        set_enabled(&session, "snippets-copy", false),
        Ok(Value::Null)
    );
    let off = info(&session, "snippets-copy");
    assert_eq!((off.enabled, off.state), (false, State::Off));
    let why = call(&session, "snippets-copy", query.clone()).unwrap_err();
    assert!(why.contains("turned off"), "{why}");
    let kept = std::fs::read_to_string(host.data_dir.join("plugins.json")).unwrap();
    assert!(kept.contains("snippets-copy"), "{kept}");

    assert_eq!(
        set_enabled(&session, "snippets-copy", true),
        Ok(Value::Null)
    );
    assert_eq!(
        call(&session, "snippets-copy", query.clone()),
        Ok(json!([]))
    );
    assert_eq!(info(&session, "snippets-copy").state, State::Running);

    // A built-in plugin has a switch too.
    set_enabled(&session, "snippets", false).unwrap();
    let why = call(&session, "snippets", query.clone()).unwrap_err();
    assert!(why.contains("Snippets is turned off"), "{why}");
    set_enabled(&session, "snippets", true).unwrap();
    assert_eq!(call(&session, "snippets", query), Ok(json!([])));

    let why = set_enabled(&session, "nothing", false).unwrap_err();
    assert!(why.contains("no plugin"), "{why}");
    drop(session);
    stop(&host);
}

#[test]
fn plugins_come_and_go_with_their_directories() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (watcher, watcher_notices) = session(&host);
    assert_eq!(ids(&list(&watcher)), ["snippets"]);

    let (session, _notices) = session(&host);
    let installed = install(&host, "copy", &snippets_copy(&host));
    assert_eq!(ids(&list(&session)), ["snippets", "snippets-copy"]);
    // The other client showing the list hears that it changed.
    next_notice(&watcher_notices, is_event_of(api::PLUGIN));

    call(
        &session,
        "snippets-copy",
        json!({"op": "list", "query": ""}),
    )
    .unwrap();
    std::fs::remove_dir_all(&installed).unwrap();
    assert_eq!(ids(&list(&session)), ["snippets"]);
    let why = call(
        &session,
        "snippets-copy",
        json!({"op": "list", "query": ""}),
    )
    .unwrap_err();
    assert!(why.contains("no plugin named"), "{why}");
    drop((watcher, session));
    stop(&host);
}

#[test]
fn a_manifest_that_cannot_be_used_is_listed_with_why() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let copy = snippets_copy(&host);
    install(&host, "copy", &copy);
    let broken = install(&host, "broken", "id = \n");
    // Not used whether let run or not: placed, as installed and never
    // turned on.
    place(&host, "twin", &copy);
    place(
        &host,
        "clash",
        &copy.replace("\"snippets-copy\"", "\"snippets\""),
    );
    place(&host, "future", &copy.replace("api = 1", "api = 99"));
    std::fs::create_dir_all(host.data_dir.join("plugins/empty")).unwrap();
    let (session, _notices) = session(&host);

    let reason = |id: &str| match info(&session, id).state {
        State::Invalid { reason } => reason,
        other => panic!("{id} is {other:?}"),
    };
    assert!(
        reason("broken").starts_with("plugin.toml, line 1"),
        "{}",
        reason("broken")
    );
    assert!(reason("empty").contains("no plugin.toml"));
    assert!(reason("future").contains("newer ThinkTerm"));
    let listed = list(&session);
    let twins: Vec<&Info> = listed
        .iter()
        .filter(|info| info.id == "snippets-copy")
        .collect();
    assert_eq!(twins.len(), 2);
    assert_eq!(
        twins
            .iter()
            .filter(|info| info.state == State::Idle)
            .count(),
        1
    );
    let shadowed = listed
        .iter()
        .filter_map(|info| info.state.reason())
        .any(|why| why.contains("copy has the same id"));
    assert!(shadowed, "{listed:?}");
    let clash = listed
        .iter()
        .filter_map(|info| info.state.reason())
        .any(|why| why.contains("belongs to a built-in plugin"));
    assert!(clash, "{listed:?}");
    // The built-in plugin still answers to its id.
    assert_eq!(
        call(&session, "snippets", json!({"op": "list", "query": ""})),
        Ok(json!([]))
    );

    std::fs::write(
        broken.join("plugin.toml"),
        copy.replace("snippets-copy", "mended"),
    )
    .unwrap();
    assert_eq!(
        info(&session, "mended").state,
        State::New,
        "another plugin now, which waits to be let run"
    );
    set_enabled(&session, "mended", true).unwrap();
    assert_eq!(info(&session, "mended").state, State::Idle);
    drop(session);
    stop(&host);
}

#[test]
fn plugins_have_no_commands_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (session, _notices) = session(&host);
    let run = json!({"op": "run", "plugin": "snippets", "command": "frobnicate"});
    let why = call(&session, api::PLUGIN, run).unwrap_err();
    assert!(why.contains("not a request the plugin host knows"), "{why}");
    drop(session);
    stop(&host);
}

#[test]
fn a_plugin_someone_watches_runs_on_unused() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, "copy", &snippets_copy(&host));
    let mut child = spawn(&host, &["--briefly-secs", "1"]);
    wait_until("the host to listen", || host.socket.exists());
    let (session, _notices) = session(&host);
    // Listing snippets watches them: the session hears when they change.
    call(
        &session,
        "snippets-copy",
        json!({"op": "list", "query": ""}),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(info(&session, "snippets-copy").state, State::Running);
    drop(session);
    stop(&host);
    wait_until("the host to quit", || child.try_wait().unwrap().is_some());
}

#[test]
fn reloading_stops_a_plugin_and_the_next_call_starts_it() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    install(&host, "copy", &snippets_copy(&host));
    let (session, _notices) = session(&host);
    let query = json!({"op": "list", "query": ""});
    call(&session, "snippets-copy", query.clone()).unwrap();
    assert_eq!(info(&session, "snippets-copy").state, State::Running);

    let reload = |id: Option<&str>| {
        manage(
            &session,
            api::Request::Reload {
                id: id.map(str::to_string),
            },
        )
    };
    assert_eq!(reload(Some("snippets-copy")), Ok(Value::Null));
    assert_eq!(info(&session, "snippets-copy").state, State::Idle);
    call(&session, "snippets-copy", query.clone()).unwrap();
    assert_eq!(reload(None), Ok(Value::Null));
    assert_eq!(info(&session, "snippets-copy").state, State::Idle);
    assert_eq!(reload(Some("snippets")), Ok(Value::Null), "nothing to do");
    assert!(reload(Some("nothing")).is_err());
    drop(session);
    stop(&host);
}

/// Plugins written as shell scripts, for the ways a program can misbehave.
#[cfg(unix)]
mod scripts {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Says it is ready; answers a call with its pid; exits with status 3
    /// when a call says "crash", and never answers one that says "hang".
    const WELL_BEHAVED: &str = r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"type":"[a-z]*","id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) exit 0 ;;
    *'"crash"'*) exit 3 ;;
    *'"hang"'*) ;;
    *) printf '{"type":"ok","id":%s,"body":%s}\n' "$id" "$$" ;;
  esac
done
"#;

    const MANIFEST: &str = r#"
id = "shell"
name = "Shell"
version = "1"
api = 1

[run]
program = "plugin.sh"
"#;

    fn install_script(host: &Host, script: &str) -> PathBuf {
        install_script_in(host, "shell", script)
    }

    /// The same plugin, id "shell", installed in directory `name`, and let
    /// run.
    fn install_script_in(host: &Host, name: &str, script: &str) -> PathBuf {
        let program = place_script(host, name, script);
        allow(host, "shell", program.parent().unwrap());
        program
    }

    /// [`install_script_in`], never let run.
    fn place_script(host: &Host, name: &str, script: &str) -> PathBuf {
        let dir = place(host, name, MANIFEST);
        let program = dir.join("plugin.sh");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    #[test]
    fn a_new_plugin_runs_only_once_turned_on_and_only_from_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let program = place_script(&host, "shell", WELL_BEHAVED);
        let (session, _notices) = session(&host);
        let shell = info(&session, "shell");
        assert_eq!((shell.state, shell.enabled), (State::New, false));
        let switches: Value =
            serde_json::from_slice(&std::fs::read(host.data_dir.join("plugins.json")).unwrap())
                .unwrap();
        assert_eq!(
            switches["plugins"]["shell"],
            json!({"enabled": false, "held": true}),
            "written in as off, for a build that does not know to wait"
        );
        let why = call(&session, "shell", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("is new"), "{why}");
        let why = set_background(&session, "shell", Some(api::Background::Always)).unwrap_err();
        assert!(why.contains("turn it on first"), "{why}");

        set_enabled(&session, "shell", true).unwrap();
        assert_eq!(info(&session, "shell").state, State::Idle);
        let first = pid(call(&session, "shell", json!({"op": "pid"})));

        // Its link pointed at another copy: it is stopped, and that one
        // waits to be let run, as a plugin of its own.
        let plugin = program.parent().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::rename(plugin, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, plugin).unwrap();
        assert_eq!(info(&session, "shell").state, State::New);
        wait_until("the run let from where it was to stop", || !alive(first));
        let why = call(&session, "shell", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("is new"), "{why}");

        // Put back where it was let run from: on again, with no new say.
        std::fs::remove_file(plugin).unwrap();
        std::fs::rename(&elsewhere, plugin).unwrap();
        let back = info(&session, "shell");
        assert_eq!((back.state, back.enabled), (State::Idle, true), "on again");
        let second = pid(call(&session, "shell", json!({"op": "pid"})));
        assert_ne!(second, first);

        // Elsewhere again, and let run from there.
        std::fs::rename(plugin, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, plugin).unwrap();
        assert_eq!(info(&session, "shell").state, State::New);
        wait_until("the second run to stop", || !alive(second));
        set_enabled(&session, "shell", true).unwrap();
        assert_ne!(pid(call(&session, "shell", json!({"op": "pid"}))), second);
        drop(session);
        stop(&host);
    }

    #[test]
    fn a_new_plugin_that_runs_always_waits_to_be_let_run_too() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let program = place_script(&host, "shell", WELL_BEHAVED);
        let manifest = program.with_file_name("plugin.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            text.replace(
                "program = \"plugin.sh\"",
                "program = \"plugin.sh\"\nbackground = \"always\"",
            ),
        )
        .unwrap();
        let mut child = spawn(&host, &[]);
        wait_until("the host to listen", || host.socket.exists());
        let mut keeper = host.connect().unwrap();
        keeper
            .send(&ToHost::Call {
                id: 1,
                plugin: api::PLUGIN.into(),
                body: json!({"op": "keep"}),
            })
            .unwrap();
        let (session, _notices) = session(&host);
        // Kept, and looked at: not started.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(info(&session, "shell").state, State::New);
        let always = thinkterm_plugin_channel::paths::always_in(&host.data_dir);
        assert!(!always.exists(), "nothing to keep the host up for");

        set_enabled(&session, "shell", true).unwrap();
        wait_until("it to start by itself", || {
            info(&session, "shell").state == State::Running
        });
        assert_eq!(std::fs::read_to_string(&always).unwrap(), "shell\n");
        drop(session);
        keeper.shutdown();
        drop(keeper);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    fn pid(answer: Answer) -> u64 {
        answer.unwrap().as_u64().expect("a pid")
    }

    #[test]
    fn a_plugin_that_keeps_stopping_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, WELL_BEHAVED);
        let (session, _notices) = session(&host);
        let first = pid(call(&session, "shell", json!({"op": "pid"})));

        for round in 1..=3 {
            let why = call(&session, "shell", json!({"op": "crash"})).unwrap_err();
            assert!(why.contains("exited with status 3"), "{why}");
            let state = info(&session, "shell").state;
            match round {
                3 => assert!(matches!(state, State::Failed { .. }), "{state:?}"),
                _ => {
                    assert!(matches!(state, State::Crashed { .. }), "{state:?}");
                    // Using it again starts it again.
                    assert_ne!(pid(call(&session, "shell", json!({"op": "pid"}))), first);
                }
            }
        }
        let why = call(&session, "shell", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("3 times within a minute"), "{why}");

        manage(
            &session,
            api::Request::Reload {
                id: Some("shell".into()),
            },
        )
        .unwrap();
        assert_eq!(info(&session, "shell").state, State::Idle);
        pid(call(&session, "shell", json!({"op": "pid"})));
        drop(session);
        stop(&host);
    }

    fn set_background(session: &Session, id: &str, background: Option<api::Background>) -> Answer {
        manage(
            session,
            api::Request::SetBackground {
                id: id.into(),
                background,
            },
        )
    }

    #[test]
    fn a_plugin_gone_unused_stops_after_as_long_as_it_may_run_so() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, WELL_BEHAVED);
        let mut child = spawn(&host, &["--briefly-secs", "2", "--never-secs", "1"]);
        wait_until("the host to listen", || host.socket.exists());
        let (session, _notices) = session(&host);
        let shell = info(&session, "shell");
        assert_eq!(
            (shell.background, shell.background_default),
            (api::Background::Briefly, api::Background::Briefly)
        );

        let first = pid(call(&session, "shell", json!({"op": "pid"})));
        assert_eq!(info(&session, "shell").state, State::Running);
        wait_until("the unused plugin to stop", || {
            info(&session, "shell").state == State::Idle
        });
        // Used again, it starts again.
        assert_ne!(pid(call(&session, "shell", json!({"op": "pid"}))), first);

        assert_eq!(
            set_background(&session, "shell", Some(api::Background::Never)),
            Ok(Value::Null)
        );
        let shell = info(&session, "shell");
        assert_eq!(
            (shell.background, shell.background_default),
            (api::Background::Never, api::Background::Briefly),
            "the user's choice, shown against the manifest's"
        );
        wait_until("it to stop sooner", || {
            info(&session, "shell").state == State::Idle
        });
        assert_eq!(set_background(&session, "shell", None), Ok(Value::Null));
        assert_eq!(info(&session, "shell").background, api::Background::Briefly);
        assert!(set_background(&session, "snippets", Some(api::Background::Never)).is_err());
        assert!(set_background(&session, "nothing", None).is_err());
        drop(session);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_call_never_answered_stops_counting_as_a_use_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, WELL_BEHAVED);
        let mut child = spawn(&host, &["--never-secs", "1", "--pending-secs", "2"]);
        wait_until("the host to listen", || host.socket.exists());
        let (session, _notices) = session(&host);
        set_background(&session, "shell", Some(api::Background::Never)).unwrap();
        // The client gives up at once; the host keeps the call waiting,
        // and no other call comes after it.
        session.call_within(
            "shell",
            json!({"op": "hang"}),
            Duration::from_millis(100),
            |_| {},
        );
        wait_until("it to start", || {
            info(&session, "shell").state == State::Running
        });
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            info(&session, "shell").state,
            State::Running,
            "used while the call waits"
        );
        wait_until("it to stop unused once the call is let go of", || {
            info(&session, "shell").state == State::Idle
        });
        drop(session);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_plugin_that_runs_always_runs_while_thinkterm_keeps_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let program = install_script(&host, WELL_BEHAVED);
        let manifest = program.with_file_name("plugin.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            text.replace(
                "program = \"plugin.sh\"",
                "program = \"plugin.sh\"\nbackground = \"always\"",
            ),
        )
        .unwrap();
        let mut child = spawn(&host, &["--briefly-secs", "1"]);
        wait_until("the host to listen", || host.socket.exists());
        let (session, _notices) = session(&host);
        let shell = info(&session, "shell");
        assert_eq!(shell.background_default, api::Background::Always);
        assert_eq!(shell.state, State::Idle, "nothing keeps ThinkTerm up yet");
        // Where ThinkTerm looks to know it is to keep the host up.
        let always = thinkterm_plugin_channel::paths::always_in(&host.data_dir);
        assert_eq!(std::fs::read_to_string(&always).unwrap(), "shell\n");

        // ThinkTerm running on the machine connects, and says so.
        let mut keeper = host.connect().unwrap();
        keeper
            .send(&ToHost::Call {
                id: 1,
                plugin: api::PLUGIN.into(),
                body: json!({"op": "keep"}),
            })
            .unwrap();
        wait_until("it to start by itself", || {
            info(&session, "shell").state == State::Running
        });
        let first = pid(call(&session, "shell", json!({"op": "pid"})));
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            info(&session, "shell").state,
            State::Running,
            "unused, and kept"
        );

        // Stopped by itself, it is started again after a pause.
        let why = call(&session, "shell", json!({"op": "crash"})).unwrap_err();
        assert!(why.contains("exited with status 3"), "{why}");
        wait_until("it to start again", || {
            info(&session, "shell").state == State::Running
        });
        assert_ne!(pid(call(&session, "shell", json!({"op": "pid"}))), first);

        // Once ThinkTerm is gone, it runs as the others do.
        keeper.shutdown();
        drop(keeper);
        wait_until("it to stop unused", || {
            info(&session, "shell").state == State::Idle
        });
        set_background(&session, "shell", Some(api::Background::Briefly)).unwrap();
        assert!(
            !always.exists(),
            "none runs always: nothing to keep the host up for"
        );
        drop(session);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_plugin_installed_to_run_always_is_found_when_thinkterm_keeps_again() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let mut child = spawn(&host, &[]);
        wait_until("the host to listen", || host.socket.exists());
        let keep = |keeper: &mut thinkterm_plugin_channel::client::Connection| {
            keeper
                .send(&ToHost::Call {
                    id: 1,
                    plugin: api::PLUGIN.into(),
                    body: json!({"op": "keep"}),
                })
                .unwrap();
        };
        let mut keeper = host.connect().unwrap();
        keep(&mut keeper);
        let always = thinkterm_plugin_channel::paths::always_in(&host.data_dir);

        // Installed while the host runs, and nothing asked for the list.
        let program = install_script(&host, WELL_BEHAVED);
        let manifest = program.with_file_name("plugin.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            text.replace(
                "program = \"plugin.sh\"",
                "program = \"plugin.sh\"\nbackground = \"always\"",
            ),
        )
        .unwrap();
        keep(&mut keeper);
        wait_until("it to be marked", || {
            std::fs::read_to_string(&always).is_ok_and(|ids| ids == "shell\n")
        });
        let (session, _notices) = session(&host);
        wait_until("it to start by itself", || {
            info(&session, "shell").state == State::Running
        });
        drop(session);
        keeper.shutdown();
        drop(keeper);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_plugin_that_never_says_it_is_ready_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, "#!/bin/sh\nexec sleep 30\n");
        let mut child = spawn(&host, &["--ready-secs", "1"]);
        wait_until("the host to listen", || host.socket.exists());
        let (session, _notices) = session(&host);
        let why = call(&session, "shell", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("did not say it was ready within 1s"), "{why}");
        assert!(matches!(
            info(&session, "shell").state,
            State::Crashed { .. }
        ));
        drop(session);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_notification_a_plugin_sends_reaches_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(
            &host,
            r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"type":"[a-z]*","id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) exit 0 ;;
    *) printf '%s\n' '{"type":"notify","title":"Look","body":"here"}'
       printf '{"type":"ok","id":%s,"body":%s}\n' "$id" "$$" ;;
  esac
done
"#,
        );
        let (session, notices) = session_listing(&host);
        pid(call(&session, "shell", json!({"op": "pid"})));
        // A list's change comes after the answer; a notification never does.
        std::thread::sleep(Duration::from_millis(500));
        while let Ok(notice) = notices.try_recv() {
            if let Notice::Event { body, .. } = notice {
                assert_ne!(body["event"], "notify", "a notification reached a client");
            }
        }
        drop(session);
        stop(&host);
    }

    /// A session that has asked for the list, and so hears what it is told.
    fn session_listing(host: &Host) -> (Session, mpsc::Receiver<Notice>) {
        let (session, notices) = session(host);
        list(&session);
        (session, notices)
    }

    /// Saves its pid when a call says "save", and again on its way out, a
    /// second after it is told to stop: a plugin writing what it holds.
    const SLOW_TO_STOP: &str = r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"type":"[a-z]*","id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) break ;;
    *'"save"'*) printf '%s' "$$" > "$THINKTERM_PLUGIN_DATA/state"
      printf '{"type":"ok","id":%s,"body":%s}\n' "$id" "$$" ;;
    *) printf '{"type":"ok","id":%s,"body":%s}\n' "$id" "$$" ;;
  esac
done
sleep 1
printf '%s' "$$" > "$THINKTERM_PLUGIN_DATA/state"
"#;

    #[test]
    fn a_reloaded_plugin_starts_again_only_once_its_last_run_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, SLOW_TO_STOP);
        let state = host.data_dir.join("plugin-data/shell/state");
        let (session, _notices) = session(&host);
        let first = pid(call(&session, "shell", json!({"op": "save"})));

        manage(
            &session,
            api::Request::Reload {
                id: Some("shell".into()),
            },
        )
        .unwrap();
        // Made at once, while the first run is still saving on its way out:
        // it waits for that run to be gone.
        let second = pid(call(&session, "shell", json!({"op": "save"})));
        assert_ne!(second, first);
        std::thread::sleep(Duration::from_millis(1500));
        let saved: u64 = std::fs::read_to_string(&state).unwrap().parse().unwrap();
        assert_eq!(saved, second, "the first run wrote over the second");
        drop(session);
        stop(&host);
    }

    #[test]
    fn a_plugin_taking_over_an_id_starts_only_once_the_old_run_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, SLOW_TO_STOP);
        let state = host.data_dir.join("plugin-data/shell/state");
        let (session, _notices) = session(&host);
        let first = pid(call(&session, "shell", json!({"op": "save"})));

        // A copy whose directory sorts first claims the id. The one that
        // ran under it is stopped, and saves on its way out into the data
        // directory the two share.
        install_script_in(&host, "a-shell", SLOW_TO_STOP);
        list(&session);
        let second = pid(call(&session, "shell", json!({"op": "save"})));
        assert_ne!(second, first);
        std::thread::sleep(Duration::from_millis(1500));
        let saved: u64 = std::fs::read_to_string(&state).unwrap().parse().unwrap();
        assert_eq!(saved, second, "the old run wrote over the new one");
        drop(session);
        stop(&host);
    }

    #[test]
    fn a_plugin_whose_id_changes_answers_to_the_new_one_only() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let program = install_script(&host, SLOW_TO_STOP);
        let (session, _notices) = session(&host);
        let first = pid(call(&session, "shell", json!({"op": "pid"})));

        let renamed = MANIFEST.replace(r#"id = "shell""#, r#"id = "renamed""#);
        std::fs::write(program.with_file_name("plugin.toml"), renamed).unwrap();
        // Made for the old id as the manifest changes: not the renamed
        // plugin's to answer.
        let why = call(&session, "shell", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("no plugin named"), "{why}");
        // Under another id it is another plugin, which waits to be let run.
        let why = call(&session, "renamed", json!({"op": "pid"})).unwrap_err();
        assert!(why.contains("is new"), "{why}");
        set_enabled(&session, "renamed", true).unwrap();
        // The old run is on its way out under the old id; the new id has
        // nothing to wait for.
        let second = pid(call(&session, "renamed", json!({"op": "pid"})));
        assert_ne!(second, first);
        drop(session);
        stop(&host);
    }

    #[test]
    fn a_plugin_another_host_turned_off_stops_here_as_a_switch_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, WELL_BEHAVED);
        let (session, _notices) = session(&host);
        let running = pid(call(&session, "shell", json!({"op": "pid"})));

        // Another host -- a debug build's -- turns it off in the file they
        // share, keeping where it was let run from; then a switch is set
        // here.
        let path = host.data_dir.join("plugins.json");
        let mut switches: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        switches["plugins"]["shell"]["enabled"] = json!(false);
        std::fs::write(&path, switches.to_string()).unwrap();
        set_enabled(&session, "snippets", false).unwrap();
        wait_until("the plugin turned off elsewhere to stop", || {
            !alive(running)
        });
        assert_eq!(info(&session, "shell").state, State::Off);
        drop(session);
        stop(&host);
    }

    fn alive(pid: u64) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_program_is_not_given_the_pane_the_host_was_started_from() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(
            &host,
            r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"type":"[a-z]*","id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) exit 0 ;;
    *) printf '{"type":"ok","id":%s,"body":"%s %s"}\n' "$id" "${WEZTERM_PANE-none}" "${WEZTERM_UNIX_SOCKET-none}" ;;
  esac
done
"#,
        );
        // Started by a command run in a pane, the host has the pane's.
        let mut child = spawn_with_env(
            &host,
            &[],
            &[
                ("WEZTERM_PANE", "7"),
                ("WEZTERM_UNIX_SOCKET", "/tmp/elsewhere"),
            ],
        );
        wait_until("the host to listen", || host.socket.exists());
        let (session, _notices) = session(&host);
        let seen = call(&session, "shell", json!({"op": "env"})).unwrap();
        assert_eq!(seen, json!("none none"));
        drop(session);
        stop(&host);
        wait_until("the host to quit", || child.try_wait().unwrap().is_some());
    }

    #[test]
    fn what_a_program_writes_to_standard_error_is_logged_under_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(
            &host,
            r#"#!/bin/sh
printf '%s\n' '{"type":"ready","api":1}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"type":"[a-z]*","id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"type":"stop"'*) exit 0 ;;
    *) printf 'looking at call %s\n' "$id" >&2
       printf '{"type":"ok","id":%s,"body":null}\n' "$id" ;;
  esac
done
"#,
        );
        let (session, _notices) = session(&host);
        call(&session, "shell", json!({"op": "look"})).unwrap();
        wait_until("the line in the host's log", || {
            std::fs::read_to_string(&host.log)
                .unwrap_or_default()
                .contains("plugin shell: looking at call 1")
        });
        drop(session);
        stop(&host);
    }

    #[test]
    fn calls_left_by_a_client_that_went_do_not_keep_the_plugin_busy() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        install_script(&host, WELL_BEHAVED);
        let mut gone = host.connect().unwrap();
        let limit = 64;
        for id in 1..=limit + 1 {
            gone.send(&ToHost::Call {
                id,
                plugin: "shell".into(),
                body: json!({"op": "hang"}),
            })
            .unwrap();
        }
        // The call past the limit is refused, so the limit is reached.
        let (tx, rx) = mpsc::channel();
        let mut reader = gone.try_clone().unwrap();
        std::thread::spawn(move || {
            while let Ok(message) = reader.recv() {
                if let FromHost::Error { id, message } = message {
                    let _ = tx.send((id, message));
                    return;
                }
            }
        });
        let (id, why) = rx
            .recv_timeout(WAIT)
            .expect("the call past the limit is refused");
        assert_eq!(id, limit + 1);
        assert!(why.contains("unanswered already"), "{why}");
        gone.shutdown();
        drop(gone);

        let (session, _notices) = session(&host);
        wait_until("the plugin to take calls again", || {
            call(&session, "shell", json!({"op": "pid"})).is_ok()
        });
        drop(session);
        stop(&host);
    }

    #[test]
    fn a_rebuilt_program_is_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path());
        let program = install_script(&host, WELL_BEHAVED);
        let (session, _notices) = session(&host);
        let first = pid(call(&session, "shell", json!({"op": "pid"})));
        assert_eq!(pid(call(&session, "shell", json!({"op": "pid"}))), first);

        std::fs::write(&program, format!("{WELL_BEHAVED}# rebuilt\n")).unwrap();
        let second = pid(call(&session, "shell", json!({"op": "pid"})));
        assert_ne!(second, first);
        assert_eq!(info(&session, "shell").state, State::Running);
        drop(session);
        stop(&host);
    }
}
