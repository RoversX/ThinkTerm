//! The host as its clients see it: a real process, started by the client
//! library, on a socket in a directory of the test's own.

mod common;

use common::{host_in, next_notice, session, spawn, stop, wait_until, WAIT};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::client::{Answer, Connection, Host, Notice, Session};
use thinkterm_plugin_channel::wire::{read_frame, write_frame, FromHost, ToHost, PROTOCOL};
use thinkterm_snippets::wire::{Event, Request, Row, Saved, PLUGIN};

fn call(connection: &mut Connection, id: u64, request: Request) {
    connection
        .send(&ToHost::Call {
            id,
            plugin: PLUGIN.into(),
            body: serde_json::to_value(request).unwrap(),
        })
        .unwrap();
}

fn answer(connection: &mut Connection, id: u64) -> Value {
    match connection.recv().unwrap() {
        FromHost::Ok { id: got, body } if got == id => body,
        other => panic!("expected the answer to {id}, got {other:?}"),
    }
}

fn event(connection: &mut Connection) -> Event {
    match connection.recv().unwrap() {
        FromHost::Event { plugin, body } if plugin == PLUGIN => {
            serde_json::from_value(body).unwrap()
        }
        other => panic!("expected an event, got {other:?}"),
    }
}

fn list(connection: &mut Connection, id: u64) -> Vec<Row> {
    call(
        connection,
        id,
        Request::List {
            query: String::new(),
        },
    );
    serde_json::from_value(answer(connection, id)).unwrap()
}

fn save(connection: &mut Connection, id: u64, body: &str) -> Saved {
    call(
        connection,
        id,
        Request::Save {
            id: None,
            title: String::new(),
            body: body.into(),
        },
    );
    serde_json::from_value(answer(connection, id)).unwrap()
}

#[test]
fn two_clients_share_one_store_and_hear_each_others_changes() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut desktop = host.connect().unwrap();
    let mut browser = host.connect().unwrap();
    assert!(list(&mut desktop, 1).is_empty());
    assert!(list(&mut browser, 1).is_empty());

    assert!(matches!(
        save(&mut desktop, 2, "ls -la"),
        Saved::Saved { .. }
    ));
    assert_eq!(event(&mut desktop), Event::Changed);
    assert_eq!(event(&mut browser), Event::Changed);
    let rows = list(&mut browser, 2);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].preview, "ls -la");

    let on_disk = thinkterm_snippets::file::load(&host.data_dir.join("snippets.json")).unwrap();
    assert_eq!(on_disk.get(&rows[0].id).unwrap().body, "ls -la");
    stop(&host);
}

#[test]
fn a_host_that_went_away_is_started_again_with_the_same_snippets() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut client = host.connect().unwrap();
    list(&mut client, 1);
    assert!(matches!(
        save(&mut client, 2, "make test"),
        Saved::Saved { .. }
    ));

    client.send(&ToHost::Quit).unwrap();
    // The event for the save may still be on its way; the end comes after it.
    wait_until("the host to go", || client.recv().is_err());
    wait_until("the socket to go", || !host.socket.exists());

    let mut again = host.connect().unwrap();
    assert_eq!(list(&mut again, 1)[0].preview, "make test");
    stop(&host);
}

#[test]
fn a_call_to_no_plugin_is_answered_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut client = host.connect().unwrap();
    client
        .send(&ToHost::Call {
            id: 9,
            plugin: "nothing".into(),
            body: json!({}),
        })
        .unwrap();
    match client.recv().unwrap() {
        FromHost::Error { id: 9, message } => assert!(message.contains("nothing"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }
    stop(&host);
}

#[test]
fn only_one_host_runs_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut first = spawn(&host, &[]);
    wait_until("the first host to listen", || host.socket.exists());
    let mut second = spawn(&host, &[]);
    let mut status = None;
    wait_until("the second host to give way", || {
        status = second.try_wait().unwrap();
        status.is_some()
    });
    assert!(status.unwrap().success());
    assert!(
        first.try_wait().unwrap().is_none(),
        "the first host keeps running"
    );
    list(&mut host.connect().unwrap(), 1);
    stop(&host);
    wait_until("the first host to quit", || {
        first.try_wait().unwrap().is_some()
    });
}

#[test]
fn the_host_exits_once_nobody_is_connected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut child = spawn(&host, &["--idle-secs", "1"]);
    wait_until("the host to listen", || host.socket.exists());
    let mut client = host.connect().unwrap();
    list(&mut client, 1);
    // Connected, it stays.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none());
    drop(client);
    wait_until("the idle host to exit", || {
        child.try_wait().unwrap().is_some()
    });
    assert!(!host.socket.exists(), "an exiting host removes its socket");
}

/// A listener at the host's socket that says hello with `protocol` and
/// hands back what it is sent first.
fn impostor(host: &Host, protocol: u32) -> std::thread::JoinHandle<Option<ToHost>> {
    std::fs::create_dir_all(host.socket.parent().unwrap()).unwrap();
    let listener = wezterm_uds::UnixListener::bind(&host.socket).unwrap();
    let socket = host.socket.clone();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        write_frame(&mut stream, &FromHost::Hello { protocol }.encode()).unwrap();
        let first = read_frame(&mut stream)
            .ok()
            .and_then(|frame| ToHost::decode(&frame).ok());
        if first == Some(ToHost::Quit) {
            // As a host does: the socket goes before the process.
            let _ = std::fs::remove_file(&socket);
        }
        first
    })
}

#[test]
fn a_host_from_an_older_build_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let older = impostor(&host, PROTOCOL - 1);
    let mut client = host.connect().unwrap();
    assert_eq!(older.join().unwrap(), Some(ToHost::Quit));
    list(&mut client, 1);
    stop(&host);
}

#[test]
fn a_host_from_a_newer_build_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let newer = impostor(&host, PROTOCOL + 1);
    let err = host.connect().unwrap_err();
    assert!(format!("{err:#}").contains("newer"), "{err:#}");
    // It was not asked to quit: the connection just closed.
    assert_eq!(newer.join().unwrap(), None);
    assert!(host.socket.exists());
}

fn ask(session: &Session, request: Request) -> Answer {
    let (tx, rx) = mpsc::channel();
    session.call(
        PLUGIN,
        serde_json::to_value(request).unwrap(),
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    rx.recv_timeout(WAIT).expect("every call is answered")
}

#[test]
fn a_session_follows_the_host_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let (session, notices) = session(&host);
    next_notice(&notices, |n| matches!(n, Notice::Connected));
    let listed = ask(
        &session,
        Request::List {
            query: String::new(),
        },
    );
    assert_eq!(listed, Ok(json!([])));
    let saved = ask(
        &session,
        Request::Save {
            id: None,
            title: "Status".into(),
            body: "git status".into(),
        },
    )
    .unwrap();
    assert_eq!(saved["outcome"], "saved");
    let Notice::Event { plugin, body } =
        next_notice(&notices, |n| matches!(n, Notice::Event { .. }))
    else {
        unreachable!()
    };
    assert_eq!(
        (plugin.as_str(), body),
        (PLUGIN, json!({"event": "changed"}))
    );

    // The host goes; the session starts another and says so.
    stop(&host);
    next_notice(&notices, |n| matches!(n, Notice::Connected));
    let rows: Vec<Row> = serde_json::from_value(
        ask(
            &session,
            Request::List {
                query: "STATUS".into(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Status");
    drop(session);
    stop(&host);
}

#[test]
fn a_session_with_no_host_to_start_answers_every_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_in(dir.path());
    host.program = dir.path().join("no-such-program");
    let (session, notices) = session(&host);
    let why = ask(
        &session,
        Request::List {
            query: String::new(),
        },
    )
    .unwrap_err();
    assert!(why.contains("no-such-program"), "{why}");
    let trouble = next_notice(&notices, |n| matches!(n, Notice::Trouble(_)));
    assert!(matches!(trouble, Notice::Trouble(why) if why.contains("no-such-program")));
}

#[test]
fn a_call_to_a_host_that_hangs_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    // Says hello, takes every call, answers none.
    let listener = wezterm_uds::UnixListener::bind(&host.socket).unwrap();
    let hung = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        write_frame(
            &mut stream,
            &FromHost::Hello { protocol: PROTOCOL }.encode(),
        )
        .unwrap();
        while read_frame(&mut stream).is_ok() {}
    });
    let (tx, notices) = mpsc::channel();
    let session = Session::start_with(host.clone(), Duration::from_millis(300), move |notice| {
        let _ = tx.send(notice);
    })
    .unwrap();
    next_notice(&notices, |n| matches!(n, Notice::Connected));
    let started = Instant::now();
    let why = ask(
        &session,
        Request::List {
            query: String::new(),
        },
    )
    .unwrap_err();
    assert!(why.contains("did not answer"), "{why}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    drop(session);
    hung.join().unwrap();
}
