//! A session can be held with no channel on it: one kept for file
//! transfers, one whose command has ended, one that has run nothing yet.
//! No channel is read then, so its socket is the session loop's to see to.

use crate::sshd::*;
use rstest::*;
use std::time::Duration;
use wezterm_ssh::{Config, DeadSession};

#[fixture]
fn sshd_asking_if_alive() -> Sshd {
    let mut config = SshdConfig::default();
    config.set_client_alive(1, 3);
    Sshd::spawn(config).unwrap()
}

#[fixture]
async fn session_asked_if_alive(
    #[future]
    #[with(Config::new(), sshd_asking_if_alive())]
    session: SessionWithSshd,
) -> SessionWithSshd {
    session.await
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn held_session_answers_the_server_with_no_channel_open(
    #[future] session_asked_if_alive: SessionWithSshd,
) {
    if !sshd_available() {
        return;
    }
    smol::block_on(async {
        let session: SessionWithSshd = session_asked_if_alive.await;

        // The server asks every second, and drops a client that has let
        // three such questions go by; well past that, the session is still
        // there to use.
        smol::Timer::after(Duration::from_secs(6)).await;
        let exec = session.exec("true", None).await;
        assert!(exec.is_ok(), "{:#}", exec.err().unwrap());
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn held_session_ends_when_its_connection_does(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    smol::block_on(async {
        let session: SessionWithSshd = session.await;

        // The command returns at once, and a second later what it left
        // behind ends the server process it ran under, and the connection
        // with it: by then no channel is left to notice.
        let mut exec = session
            .exec(
                "(sleep 1; kill -9 $PPID) </dev/null >/dev/null 2>&1 &",
                None,
            )
            .await
            .unwrap();
        let _ = exec.child.async_wait().await;
        drop(exec);
        smol::Timer::after(Duration::from_secs(3)).await;

        // The session went with its connection, rather than staying for
        // whoever asks next to find it broken.
        let err = session
            .exec("true", None)
            .await
            .err()
            .expect("a session whose connection ended to refuse a command");
        assert!(err.downcast_ref::<DeadSession>().is_some(), "{:#}", err);
    })
}
