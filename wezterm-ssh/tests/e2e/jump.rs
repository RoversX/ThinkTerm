//! Reaching a server through another, as `ssh -J` does: the first server
//! carries the connection to the second in a direct-tcpip channel, and
//! each signs in with its own identity.

use crate::sshd::*;
use rstest::*;
use std::io::Read;
use std::time::{Duration, Instant};
use wezterm_ssh::{set_jump_hosts, Config, ConfigMap, JumpHop, Session, SessionEvent, JUMP_TAG};

fn sshd_named(name: &str) -> Sshd {
    let mut config = SshdConfig::default();
    config.set_env("WHICH_SSHD", name);
    Sshd::spawn(config).unwrap()
}

/// What signing in asked about, in order.
#[derive(Default, Debug)]
struct SignIn {
    /// Whose host key each question was about: `None` for the host's own.
    host_keys: Vec<Option<JumpHop>>,
    banners: Vec<String>,
}

/// Signs in, trusting every host key and answering every prompt with
/// nothing (the servers take keys only).
async fn sign_in(events: &smol::channel::Receiver<SessionEvent>) -> Result<SignIn, String> {
    let mut seen = SignIn::default();
    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::Banner(banner) => seen.banners.extend(banner),
            SessionEvent::HostVerify(verify) => {
                seen.host_keys.push(verify.jump.clone());
                verify.answer(true).await.map_err(|err| err.to_string())?;
            }
            SessionEvent::Authenticate(auth) => {
                let answers = vec![String::new(); auth.prompts.len()];
                auth.answer(answers).await.map_err(|err| err.to_string())?;
            }
            SessionEvent::HostVerificationFailed(failed) => return Err(failed.to_string()),
            SessionEvent::Error(err) | SessionEvent::AuthenticationRefused(err) => return Err(err),
            SessionEvent::Authenticated => return Ok(seen),
        }
    }
    Err("the session ended before signing in".to_string())
}

async fn run(session: &Session, command: &str) -> String {
    let mut exec = session.exec(command, None).await.unwrap();
    let mut output = String::new();
    exec.stdout.read_to_string(&mut output).unwrap();
    let _ = exec.child.async_wait().await;
    output.trim().to_string()
}

/// Whether this build has `backend`: a build may have only one.
fn compiled(backend: &str) -> bool {
    match backend {
        "libssh" => cfg!(feature = "libssh-rs"),
        "ssh2" => cfg!(feature = "ssh2"),
        _ => false,
    }
}

/// A backend this build has, for the tests any backend will do for.
fn some_backend() -> &'static str {
    if compiled("libssh") {
        "libssh"
    } else {
        "ssh2"
    }
}

/// An empty known hosts file of each's own, so that each is asked about:
/// with none at all, ssh2 asks about nothing.
fn trust_files(servers: &[&Sshd]) {
    for sshd in servers {
        std::fs::write(sshd.tmp.path().join("known_hosts"), "").unwrap();
    }
}

/// `target`'s configuration, reached through `jump`, going with `backend`.
fn through(jump: &Sshd, target: &Sshd, backend: &str) -> ConfigMap {
    trust_files(&[jump, target]);
    let mut hop = jump.client_config(&Config::new());
    hop.insert(JUMP_TAG.to_string(), "the jump".to_string());
    let mut config = target.client_config(&Config::new());
    config.insert("wezterm_ssh_backend".to_string(), backend.to_string());
    set_jump_hosts(&mut config, &[hop]);
    config
}

#[rstest]
#[case::libssh("libssh")]
#[case::ssh2("ssh2")]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_host_is_reached_through_its_jump_host(#[case] backend: &str) {
    if !sshd_available() || !compiled(backend) {
        return;
    }
    let jump = sshd_named("jump");
    let target = sshd_named("target");
    smol::block_on(async {
        let (session, events) = Session::connect(through(&jump, &target, backend)).unwrap();
        let seen = sign_in(&events).await.unwrap();

        // What runs, runs on the host beyond, not on the one in between,
        // and more than once over the one connection.
        assert_eq!(run(&session, "echo $WHICH_SSHD").await, "target");
        assert_eq!(
            run(&session, "echo again $WHICH_SSHD").await,
            "again target"
        );

        // Each host's key was asked about, the jump host's marked as its
        // own, so a key is never trusted for the wrong host.
        let asked: Vec<_> = seen
            .host_keys
            .iter()
            .map(|hop| hop.as_ref().map(|hop| (hop.index, hop.tag.clone())))
            .collect();
        assert_eq!(asked, [Some((0, Some("the jump".to_string()))), None]);
        assert!(
            seen.banners
                .iter()
                .any(|banner| banner.contains("connecting through it to localhost:")),
            "{:?}",
            seen.banners
        );

        // Much more than one channel buffer comes through intact, counted
        // here, at this end of the tunnel.
        let mut exec = session
            .exec("head -c 3000000 /dev/zero | tr '\\0' x", None)
            .await
            .unwrap();
        let mut payload = vec![];
        exec.stdout.read_to_end(&mut payload).unwrap();
        let _ = exec.child.async_wait().await;
        assert_eq!(payload.len(), 3_000_000);
        assert!(payload.iter().all(|byte| *byte == b'x'));

        // Files go the same way, as the Files panel takes them.
        let dir = wezterm_ssh::Utf8PathBuf::from_path_buf(target.tmp.path().to_path_buf()).unwrap();
        let listed = session.sftp().read_dir(dir).await.unwrap();
        assert!(
            listed
                .iter()
                .any(|(path, _)| path.file_name() == Some("sshd_config")),
            "{:?}",
            listed
        );
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_proxy_jump_option_is_resolved_and_followed() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    let target = sshd_named("target");
    trust_files(&[&jump, &target]);
    smol::block_on(async {
        let mut config = Config::new();
        config.add_config_string(&jump.host_entry("the-jump"));
        let mut host = target.client_config(&config);
        host.insert("proxyjump".to_string(), "the-jump".to_string());
        config.resolve_jump_hosts(&mut host).unwrap();

        let (session, events) = Session::connect(host).unwrap();
        let seen = sign_in(&events).await.unwrap();
        assert_eq!(run(&session, "echo $WHICH_SSHD").await, "target");
        // Named as the configuration resolved it, and with no tag: nobody
        // gave it one.
        let first = seen.host_keys[0].clone().expect("the jump host's key");
        assert_eq!(first.tag, None);
        assert!(
            first.name.ends_with(&format!("@127.0.0.1:{}", jump.port)),
            "{}",
            first.name
        );
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_jump_host_that_cannot_reach_the_host_says_so() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    smol::block_on(async {
        let mut config = through(&jump, &jump, some_backend());
        config.insert("hostname".to_string(), "127.0.0.1".to_string());
        config.insert("port".to_string(), closed.to_string());
        let (_session, events) = Session::connect(config).unwrap();
        let err = sign_in(&events).await.unwrap_err();
        assert!(
            err.contains(&format!("could not connect to 127.0.0.1:{closed}")),
            "{}",
            err
        );
        assert!(err.contains("jump host"), "{}", err);
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_jump_host_that_turns_us_away_is_named() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    let target = sshd_named("target");
    smol::block_on(async {
        let mut config = through(&jump, &target, some_backend());
        // The target's key, which the jump host does not take.
        let wrong = config["identityfile"].clone();
        config.insert("wezterm_jump.0.identityfile".to_string(), wrong);
        let (_session, events) = Session::connect(config).unwrap();
        let err = sign_in(&events).await.unwrap_err();
        assert!(err.contains("jump host"), "{}", err);
    })
}

#[rstest]
#[case::libssh("libssh")]
#[case::ssh2("ssh2")]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn shutting_a_session_down_stops_its_jump_hosts_too(#[case] backend: &str) {
    if !compiled(backend) {
        return;
    }
    // Takes connections and never says a word of SSH: signing in to it
    // waits until something gives up.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut hop = ConfigMap::new();
    hop.insert("hostname".to_string(), "127.0.0.1".to_string());
    hop.insert(
        "port".to_string(),
        silent.local_addr().unwrap().port().to_string(),
    );
    hop.insert("user".to_string(), "nobody".to_string());
    let mut config = ConfigMap::new();
    config.insert("hostname".to_string(), "beyond.invalid".to_string());
    config.insert("port".to_string(), "22".to_string());
    config.insert("user".to_string(), "nobody".to_string());
    config.insert("wezterm_ssh_backend".to_string(), backend.to_string());
    set_jump_hosts(&mut config, &[hop]);

    smol::block_on(async {
        let (session, events) = Session::connect(config).unwrap();
        smol::Timer::after(Duration::from_millis(500)).await;
        let stopped = Instant::now();
        session.shutdown();
        let outcome = smol::future::or(async { Some(sign_in(&events).await) }, async {
            smol::Timer::after(Duration::from_secs(5)).await;
            None
        })
        .await;
        let outcome = outcome.expect("the attempt to end once the session is shut down");
        assert!(outcome.is_err(), "{:?}", outcome);
        assert!(stopped.elapsed() < Duration::from_secs(5));
    });
    drop(silent);
}

#[rstest]
#[case::libssh("libssh")]
#[case::ssh2("ssh2")]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_tunnel_carries_each_way_until_that_way_ends(#[case] backend: &str) {
    if !sshd_available() || !compiled(backend) {
        return;
    }
    use std::io::Write;
    // Reads all it is sent, then answers: it hears the end of what we send
    // only if our half-close gets there, and we hear its answer only if our
    // way back stays open after it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = vec![];
        stream.read_to_end(&mut got).unwrap();
        stream
            .write_all(format!("got {} bytes", got.len()).as_bytes())
            .unwrap();
    });

    let jump = sshd_named("jump");
    trust_files(&[&jump]);
    let mut config = jump.client_config(&Config::new());
    config.insert("wezterm_ssh_backend".to_string(), backend.to_string());
    smol::block_on(async {
        let (session, events) = Session::connect(config).unwrap();
        sign_in(&events).await.unwrap();
        let socket = session.open_direct_tcpip("127.0.0.1", port).await.unwrap();
        let mut stream = std::net::TcpStream::from(socket);
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(&vec![b'x'; 200_000]).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "got 200000 bytes");
    });
    server.join().unwrap();
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn the_hosts_other_addresses_are_tried_through_its_jump_host() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    let target = sshd_named("target");
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    smol::block_on(async {
        let mut config = through(&jump, &target, some_backend());
        // Its own address does not answer; one of its others does.
        config.insert("hostname".to_string(), "127.0.0.1".to_string());
        config.insert("port".to_string(), closed.to_string());
        config.insert(
            wezterm_ssh::OTHER_ADDRESSES.to_string(),
            format!("localhost:{}", target.port),
        );
        let (session, events) = Session::connect(config).unwrap();
        let seen = sign_in(&events).await.unwrap();
        assert_eq!(run(&session, "echo $WHICH_SSHD").await, "target");
        assert!(
            seen.banners.iter().any(|banner| banner.contains(&format!(
                "connecting through it to localhost:{}",
                target.port
            ))),
            "{:?}",
            seen.banners
        );
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn a_jump_host_that_cannot_get_through_is_given_up_on_in_time() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    smol::block_on(async {
        let mut config = through(&jump, &jump, some_backend());
        // TEST-NET-1: nothing answers there, so the jump host would wait as
        // long as its own TCP connect does.
        config.insert("hostname".to_string(), "192.0.2.1".to_string());
        config.insert("connecttimeout".to_string(), "2".to_string());
        let started = Instant::now();
        let (_session, events) = Session::connect(config).unwrap();
        let err = sign_in(&events).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert!(err.contains("192.0.2.1"), "{}", err);
    })
}

#[test]
fn a_host_kept_to_the_tailnet_is_not_reached_through_a_jump_host() {
    // Refused before anything is dialed: no server is needed to tell.
    let mut hop = ConfigMap::new();
    hop.insert("hostname".to_string(), "127.0.0.1".to_string());
    hop.insert("port".to_string(), "9".to_string());
    hop.insert("user".to_string(), "nobody".to_string());
    let mut config = ConfigMap::new();
    config.insert("hostname".to_string(), "inside.invalid".to_string());
    config.insert("port".to_string(), "22".to_string());
    config.insert("user".to_string(), "nobody".to_string());
    config.insert(
        "wezterm_ssh_backend".to_string(),
        some_backend().to_string(),
    );
    config.insert(wezterm_ssh::NETWORK.to_string(), "tailnet".to_string());
    set_jump_hosts(&mut config, &[hop]);
    smol::block_on(async {
        let (_session, events) = Session::connect(config).unwrap();
        let err = sign_in(&events).await.unwrap_err();
        assert!(err.contains("built-in tailnet"), "{}", err);
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn an_address_that_times_out_through_the_jump_host_leaves_the_others_to_try() {
    if !sshd_available() {
        return;
    }
    let jump = sshd_named("jump");
    let target = sshd_named("target");
    smol::block_on(async {
        let mut config = through(&jump, &target, some_backend());
        // Its own address answers nothing (TEST-NET-1); another does.
        config.insert("hostname".to_string(), "192.0.2.1".to_string());
        config.insert("port".to_string(), "22".to_string());
        config.insert("connecttimeout".to_string(), "2".to_string());
        config.insert(
            wezterm_ssh::OTHER_ADDRESSES.to_string(),
            format!("localhost:{}", target.port),
        );
        let (session, events) = Session::connect(config).unwrap();
        sign_in(&events).await.unwrap();
        assert_eq!(run(&session, "echo $WHICH_SSHD").await, "target");
    })
}

#[test]
fn a_jump_host_kept_to_the_tailnet_is_not_reached_through_another() {
    // Refused before anything is dialed: no server is needed to tell.
    let hop = |host: &str, network: &str| {
        let mut hop = ConfigMap::new();
        hop.insert("hostname".to_string(), host.to_string());
        hop.insert("port".to_string(), "9".to_string());
        hop.insert("user".to_string(), "nobody".to_string());
        hop.insert(wezterm_ssh::NETWORK.to_string(), network.to_string());
        hop
    };
    let mut config = ConfigMap::new();
    config.insert("hostname".to_string(), "inside.invalid".to_string());
    config.insert("port".to_string(), "22".to_string());
    config.insert("user".to_string(), "nobody".to_string());
    config.insert(
        "wezterm_ssh_backend".to_string(),
        some_backend().to_string(),
    );
    set_jump_hosts(
        &mut config,
        &[
            hop("127.0.0.1", "system"),
            hop("tailnet-only.invalid", "tailnet"),
        ],
    );
    smol::block_on(async {
        let (_session, events) = Session::connect(config).unwrap();
        let err = sign_in(&events).await.unwrap_err();
        assert!(err.contains("tailnet-only.invalid"), "{}", err);
        assert!(err.contains("built-in tailnet"), "{}", err);
    })
}
