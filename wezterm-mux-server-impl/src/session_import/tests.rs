use super::*;
use std::io::{Read, Write};
use std::os::fd::AsFd;

#[test]
fn imported_readers_wait_for_output_without_a_wake_pipe() {
    use std::sync::mpsc;
    use std::time::Duration;

    let pty = nix::pty::openpty(None, None).unwrap();
    let flags = rustix::fs::fcntl_getfl(&pty.master).unwrap() | rustix::fs::OFlags::NONBLOCK;
    rustix::fs::fcntl_setfl(&pty.master, flags).unwrap();
    let mut slave = std::fs::File::from(pty.slave);
    let master = pty::ImportedPty::prepare(pty.master.try_clone().unwrap()).unwrap();
    // Exercise both the reserved reader and a later cloned reader directly,
    // as the mux does when it cannot allocate its poll wake pipe.
    for _ in 0..2 {
        let mut reader = master.try_clone_reader().unwrap();
        let (started, ready) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            let mut bytes = [0; 32];
            let result = reader.read(&mut bytes).map(|count| bytes[..count].to_vec());
            sent.send(result).ok();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_output = received.recv_timeout(Duration::from_millis(50));
        slave.write_all(b"example output").unwrap();
        assert!(matches!(
            before_output,
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            b"example output"
        );
        worker.join().unwrap();
        assert_eq!(rustix::fs::fcntl_getfl(&pty.master).unwrap(), flags);
    }
    let mut reader = master.try_clone_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let worker = std::thread::spawn(move || sent.send(reader.read(&mut [0; 32])).ok());
    drop(slave);
    match received.recv_timeout(Duration::from_secs(5)).unwrap() {
        Ok(0) => {}
        Err(err) if err.raw_os_error() == Some(libc::EIO) => {}
        result => panic!("unexpected PTY hangup: {:?}", result),
    }
    worker.join().unwrap();
}

#[test]
fn rollback_closes_reserved_writers_without_typing_eof() {
    let pty = nix::pty::openpty(None, None).unwrap();
    let source_flags = rustix::fs::fcntl_getfl(&pty.master).unwrap() | rustix::fs::OFlags::NONBLOCK;
    rustix::fs::fcntl_setfl(&pty.master, source_flags).unwrap();
    let mut slave = std::fs::File::from(pty.slave);
    let mut modes = rustix::termios::tcgetattr(&slave).unwrap();
    modes.make_raw();
    rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &modes).unwrap();
    let master = pty::ImportedPty::prepare(pty.master.try_clone().unwrap()).unwrap();
    drop(master.take_writer().unwrap());
    drop(master);
    assert_eq!(rustix::fs::fcntl_getfl(&pty.master).unwrap(), source_flags);
    let flags = rustix::fs::fcntl_getfl(&slave).unwrap();
    rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut input = [0; 32];
    assert_eq!(
        slave.read(&mut input).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(pty.master);
}

#[test]
fn imported_writer_delivers_large_queued_input_after_backpressure() {
    use std::io::ErrorKind;
    use std::time::{Duration, Instant};

    let pty = nix::pty::openpty(None, None).unwrap();
    let mut slave = std::fs::File::from(pty.slave);
    let mut modes = rustix::termios::tcgetattr(&slave).unwrap();
    modes.make_raw();
    rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &modes).unwrap();
    for fd in [pty.master.as_fd(), slave.as_fd()] {
        let flags = rustix::fs::fcntl_getfl(fd).unwrap();
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    }
    // Fill the input queue before starting the terminal's writer thread.
    let mut source = std::fs::File::from(pty.master.try_clone().unwrap());
    let mut expected = Vec::new();
    loop {
        match source.write(&[b'x'; 8192]) {
            Ok(count) => expected.resize(expected.len() + count, b'x'),
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            result => panic!("fill PTY: {:?}", result),
        }
        assert!(expected.len() < 4 * 1024 * 1024);
    }
    let master = pty::ImportedPty::prepare(pty.master).unwrap();
    let terminal = Terminal::new(
        TerminalSize::default(),
        Arc::new(config::TermConfig::with_config(
            config::ConfigHandle::default_config(),
        )),
        "ThinkTerm",
        "test",
        master.take_writer().unwrap(),
    );
    let mut writer = terminal.writer_handle();
    let payload = vec![b'a'; 1024 * 1024];
    writer.write_all(&payload).unwrap();
    writer.write_all(b"queued marker").unwrap();
    expected.extend_from_slice(&payload);
    expected.extend_from_slice(b"queued marker");
    std::thread::sleep(Duration::from_millis(50));

    let mut read_exact_input = |length| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = Vec::new();
        let mut buf = [0; 16384];
        while received.len() < length {
            assert!(
                Instant::now() < deadline,
                "input stopped at {} of {} bytes",
                received.len(),
                length
            );
            match slave.read(&mut buf) {
                Ok(0) => panic!("PTY closed before all input arrived"),
                Ok(count) => received.extend_from_slice(&buf[..count]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => panic!("read input: {}", err),
            }
        }
        received
    };
    assert_eq!(read_exact_input(expected.len()), expected);
    writer.write_all(b"still writable").unwrap();
    assert_eq!(read_exact_input(14), b"still writable");
}

fn plan() -> ImportPlan {
    use thinkterm_import::{ImportMode, Project, Selection, Thread};
    let tab = thinkterm_import::Tab {
        name: Some("build".into()),
        layout: Layout::Split {
            direction: Direction::Horizontal,
            ratio: 0.3,
            first: Box::new(Layout::Stack {
                panes: vec![1, 3],
                active: 1,
            }),
            second: Box::new(Layout::Pane(2)),
        },
        panes: [1, 2, 3]
            .iter()
            .copied()
            .map(|id| {
                (
                    id,
                    thinkterm_import::Pane {
                        cwd: "/home/user/example".into(),
                        title: None,
                    },
                )
            })
            .collect(),
        focused: Some(2),
        zoomed: true,
    };
    let mut other = tab.clone();
    other.layout = Layout::Pane(4);
    other.panes = [(
        4,
        thinkterm_import::Pane {
            cwd: "/home/user/example".into(),
            title: None,
        },
    )]
    .into();
    other.focused = None;
    other.zoomed = false;
    ImportPlan {
        mode: ImportMode::Layout,
        active: Selection {
            project: 0,
            thread: 1,
        },
        projects: vec![Project {
            name: "example".into(),
            directory: "/home/user/example".into(),
            threads: vec![
                Thread {
                    name: "development".into(),
                    tabs: vec![tab],
                    active_tab: 0,
                },
                Thread {
                    name: "logs".into(),
                    tabs: vec![other],
                    active_tab: 0,
                },
            ],
        }],
    }
}
#[test]
fn imports_tree_without_source_launch_commands() {
    let snapshot = plan();
    snapshot.validate().unwrap();
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("must-not"));
    let tree = make_tree(&snapshot, "Example import");
    assert_eq!(tree.spaces.len(), 1);
    assert_eq!(tree.projects.len(), 1);
    assert_eq!(tree.projects[0].threads.len(), 2);
    assert_eq!(snapshot.projects[0].threads[0].tabs.len(), 1);
    assert_eq!(snapshot.pane_count(), 4);
    let mut current = ThinkTermTree::default();
    let existing = codec::TtSpace {
        id: "existing".into(),
        name: "Existing".into(),
    };
    current.spaces.push(existing.clone());
    for op in tree_ops(&tree, true) {
        codec::apply_op(&mut current, &op);
    }
    assert_eq!(current.spaces.len(), 2);
    assert_eq!(current.projects.len(), 1);
    for op in tree_ops(&tree, false) {
        codec::apply_op(&mut current, &op);
    }
    assert_eq!(current.spaces, vec![existing]);
    assert!(current.projects.is_empty());
    assert_ne!(
        tree.spaces[0].id,
        make_tree(&snapshot, "Example import").spaces[0].id
    );
}

#[test]
fn preserves_split_direction_ratio_focus_and_zoom() {
    let snapshot = plan();
    let saved = &snapshot.projects[0].threads[0].tabs[0];
    let size = TerminalSize {
        cols: 101,
        rows: 30,
        ..TerminalSize::default()
    };
    let entries = saved
        .panes
        .keys()
        .map(|id| {
            (
                *id,
                PaneEntry {
                    pane_id: *id as usize + 100,
                    window_id: 1,
                    tab_id: 2,
                    title: String::new(),
                    size,
                    working_dir: None,
                    is_active_pane: *id == 2,
                    is_zoomed_pane: *id == 2,
                    alt_screen: false,
                    workspace: "example".into(),
                    cursor_pos: Default::default(),
                    physical_top: 0,
                    top_row: 0,
                    left_col: 0,
                    tty_name: None,
                },
            )
        })
        .collect();
    let tree = layout_tree(&saved.layout, size, &entries);
    assert!(tree.is_plausible());
    if let PaneNode::Split { node, left, right } = tree {
        assert_eq!(node.direction, SplitDirection::Horizontal);
        assert_eq!((node.first.cols, node.second.cols), (30, 70));
        assert!(!left.entries()[0].is_active_pane);
        if let PaneNode::Stack(stack) = *left {
            assert_eq!(stack.active, 1);
            assert_eq!(
                stack.panes.iter().map(|p| p.pane_id).collect::<Vec<_>>(),
                vec![101, 103]
            );
        } else {
            panic!("missing stack");
        }
        assert!(right.entries()[0].is_active_pane);
        assert!(right.entries()[0].is_zoomed_pane);
    } else {
        panic!("missing split");
    }
}
