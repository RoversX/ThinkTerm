//! Real SSH exchanges against a loopback-only, synthetic account. Client
//! configuration, host trust and throwaway identities stay in a temporary
//! directory; neither backend reads the user's SSH configuration or keys.

use crate::authentication::{AuthenticationBackend, AuthenticationInfo, AuthenticationMethod};
use crate::session::{SessionEvent, SessionShutdown};
use crate::sessioninner::SessionInner;
use crate::ConfigMap;
use anyhow::Context;
use filedescriptor::socketpair;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Handler};
use russh::{MethodKind, MethodSet};
use smol::channel::{bounded, Receiver};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Password,
    Interactive,
    PublicKeyThenPassword,
    RejectedKeyThenPassword,
    PasswordThenInteractive,
    EncryptedKey,
}

struct TestServer {
    scenario: Scenario,
    first_step: bool,
    credential_attempts: Arc<AtomicUsize>,
}

fn methods(methods: &[MethodKind]) -> MethodSet {
    methods.into()
}

impl TestServer {
    fn available_methods(&self) -> MethodSet {
        use MethodKind::*;
        match (self.scenario, self.first_step) {
            (Scenario::Interactive, _) | (Scenario::PasswordThenInteractive, true) => {
                methods(&[KeyboardInteractive])
            }
            (Scenario::PublicKeyThenPassword, false)
            | (Scenario::RejectedKeyThenPassword, false)
            | (Scenario::EncryptedKey, false) => methods(&[PublicKey, Password]),
            _ => methods(&[Password]),
        }
    }
}

impl Handler for TestServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(self.available_methods()),
            partial_success: false,
        })
    }

    async fn auth_publickey(&mut self, _: &str, _: &PublicKey) -> Result<Auth, Self::Error> {
        self.credential_attempts.fetch_add(1, Ordering::SeqCst);
        self.first_step = true;
        Ok(Auth::Reject {
            proceed_with_methods: Some(methods(&[MethodKind::Password])),
            partial_success: self.scenario == Scenario::PublicKeyThenPassword,
        })
    }

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        self.credential_attempts.fetch_add(1, Ordering::SeqCst);
        assert!(password == "example-password");
        if self.scenario == Scenario::PasswordThenInteractive {
            self.first_step = true;
            Ok(Auth::Reject {
                proceed_with_methods: Some(methods(&[MethodKind::KeyboardInteractive])),
                partial_success: true,
            })
        } else {
            Ok(Auth::Accept)
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if let Some(mut response) = response {
            self.credential_attempts.fetch_add(1, Ordering::SeqCst);
            assert!(response
                .next()
                .is_some_and(|value| value.as_ref() == b"example-password"));
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Partial {
                name: Cow::Borrowed("Example authentication"),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Password: "), false)]),
            })
        }
    }
}

fn test_inner(root: &Path) -> (SessionInner, Receiver<SessionEvent>) {
    let (tx_event, events) = bounded(8);
    let (_requests, rx_req) = bounded(8);
    let (_write, sender_read) = socketpair().unwrap();
    let config = ConfigMap::from([
        (
            "identityfile".into(),
            root.join("identity").to_str().unwrap().into(),
        ),
        ("identitiesonly".into(), "yes".into()),
        (
            "userknownhostsfile".into(),
            root.join("known_hosts").to_str().unwrap().into(),
        ),
    ]);
    (
        SessionInner {
            config,
            tx_event,
            rx_req,
            channels: HashMap::new(),
            files: HashMap::new(),
            dirs: HashMap::new(),
            next_channel_id: 1,
            next_file_id: 1,
            sender_read,
            session_was_dropped: false,
            shown_accept_env_error: false,
            last_keep_alive: Instant::now(),
            keep_alive: None,
            shutdown: Arc::new(SessionShutdown::default()),
            authentication: Arc::new(OnceLock::new()),
        },
        events,
    )
}

fn backends() -> Vec<AuthenticationBackend> {
    vec![
        #[cfg(feature = "libssh-rs")]
        AuthenticationBackend::LibSsh,
        #[cfg(feature = "ssh2")]
        AuthenticationBackend::Ssh2,
    ]
}

struct PartialAuthServer {
    child: Child,
    output: BufReader<ChildStdout>,
}

impl PartialAuthServer {
    fn start(root: &Path, scenario: Scenario) -> (Self, SocketAddr) {
        let python = std::env::var_os("THINKTERM_AUTH_TEST_PYTHON")
            .expect("set THINKTERM_AUTH_TEST_PYTHON to a virtualenv Python with Paramiko");
        let mut child = Command::new(python)
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/authentication-server.py"),
            )
            .arg(match scenario {
                Scenario::PublicKeyThenPassword => "key-password",
                Scenario::PasswordThenInteractive => "password-interactive",
                _ => unreachable!(),
            })
            .env_clear()
            .env("HOME", root)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self { child, output };
        let mut port = String::new();
        server.output.read_line(&mut port).unwrap();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port.trim().parse::<u16>().unwrap()));
        (server, address)
    }

    fn attempts(&mut self) -> usize {
        let mut attempts = String::new();
        self.output.read_line(&mut attempts).unwrap();
        assert!(self.child.wait().unwrap().success());
        attempts.trim().parse().unwrap()
    }
}

impl Drop for PartialAuthServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn exchange(
    backend: AuthenticationBackend,
    scenario: Scenario,
    trust_host: bool,
    cancel_password: bool,
) -> (anyhow::Result<AuthenticationInfo>, usize) {
    let directory = assert_fs::TempDir::new().unwrap();
    // Synthetic, public test material, generated without external key files.
    let server_key = PrivateKey::from(Ed25519Keypair::from_seed(&[1; 32]));
    let client_key = PrivateKey::from(Ed25519Keypair::from_seed(&[2; 32]));
    std::fs::write(
        directory.path().join("identity"),
        client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    std::fs::set_permissions(
        directory.path().join("identity"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    if scenario == Scenario::EncryptedKey {
        assert!(Command::new("ssh-keygen")
            .args(["-p", "-P", "", "-N", "example-passphrase", "-f"])
            .arg(directory.path().join("identity"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(directory.path().join("known_hosts"), "").unwrap();
    let (mut inner, events) = test_inner(directory.path());
    let root = directory.path().to_path_buf();
    let attempts = Arc::new(AtomicUsize::new(0));
    let handler = TestServer {
        scenario,
        first_step: false,
        credential_attempts: attempts.clone(),
    };
    let config = Arc::new(russh::server::Config {
        keys: vec![server_key],
        methods: handler.available_methods(),
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(5)),
        ..Default::default()
    });
    // russh 0.63.3 clears partial_success in its password/public-key server
    // paths. Use an independent server for those wire-level cases.
    let (server, mut partial_server, address) = if matches!(
        scenario,
        Scenario::PublicKeyThenPassword | Scenario::PasswordThenInteractive
    ) {
        let (server, address) = PartialAuthServer::start(directory.path(), scenario);
        (None, Some(server), address)
    } else {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                let _ = session.await;
            }
        });
        (Some(server), None, address)
    };
    let answers = tokio::spawn(async move {
        let mut prompt_count = 0;
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::HostVerify(event) => event.answer(trust_host).await.unwrap(),
                SessionEvent::Authenticate(event) => {
                    if cancel_password {
                        assert_eq!(
                            prompt_count, 0,
                            "authentication continued after cancellation"
                        );
                    }
                    prompt_count += 1;
                    let values = if cancel_password {
                        vec![]
                    } else {
                        vec!["example-password".to_string(); event.prompts.len()]
                    };
                    event.answer(values).await.unwrap();
                }
                _ => {}
            }
        }
    });
    let client = tokio::task::spawn_blocking(move || -> anyhow::Result<AuthenticationInfo> {
        match backend {
            #[cfg(feature = "libssh-rs")]
            AuthenticationBackend::LibSsh => {
                use libssh_rs::SshOption;
                let session = libssh_rs::Session::new()?;
                session.set_option(SshOption::ProcessConfig(false))?;
                session.set_option(SshOption::SshDir(Some(root.to_str().unwrap().into())))?;
                session.set_option(SshOption::KnownHosts(Some(
                    root.join("known_hosts").to_str().unwrap().into(),
                )))?;
                session.set_option(SshOption::GlobalKnownHosts(Some(
                    root.join("global_known_hosts").to_str().unwrap().into(),
                )))?;
                session.set_option(SshOption::IdentityAgent(Some(
                    root.join("no-agent").to_str().unwrap().into(),
                )))?;
                session.set_option(SshOption::AddIdentity(
                    root.join("identity").to_str().unwrap().into(),
                ))?;
                session.set_option(SshOption::Hostname(Ipv4Addr::LOCALHOST.to_string()))?;
                session.set_option(SshOption::Port(address.port()))?;
                session.set_option(SshOption::User(Some("example".into())))?;
                session.set_option(SshOption::Timeout(Duration::from_secs(3)))?;
                session.connect()?;
                let verified =
                    inner.host_verification_libssh(&session, "localhost", address.port())?;
                Ok(inner.authenticate_libssh(&session)?.finish(verified))
            }
            #[cfg(feature = "ssh2")]
            AuthenticationBackend::Ssh2 => {
                let stream =
                    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
                let mut session = ssh2::Session::new()?;
                session.set_timeout(3000);
                session.set_tcp_stream(stream);
                session.handshake()?;
                let verified =
                    inner.host_verification(&session, "localhost", address.port(), "localhost")?;
                Ok(inner
                    .authenticate(&session, "example", "localhost")?
                    .finish(verified))
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .context("authentication test exceeded its deadline")
        .unwrap()
        .unwrap();
    answers.await.unwrap();
    if let Some(server) = server {
        server.abort();
        let _ = server.await;
    }
    let attempts = if let Some(server) = partial_server.as_mut() {
        assert!(
            result.is_ok(),
            "partial-authentication client failed: {:?}",
            result
        );
        server.attempts()
    } else {
        attempts.load(Ordering::SeqCst)
    };
    (result, attempts)
}

#[tokio::test]
async fn real_password_exchange_has_verified_connection_evidence() {
    for backend in backends() {
        let (result, attempts) = exchange(backend, Scenario::Password, true, false).await;
        let info = result.unwrap();
        assert_eq!(info.backend(), backend);
        assert_eq!(info.methods(), &[AuthenticationMethod::Password]);
        assert!(info.is_verified_password_only());
        assert_eq!(attempts, 1);
    }
}

#[tokio::test]
async fn real_password_prompt_can_be_keyboard_interactive() {
    for backend in backends() {
        let (result, _) = exchange(backend, Scenario::Interactive, true, false).await;
        let info = result.unwrap();
        assert_eq!(info.methods(), &[AuthenticationMethod::KeyboardInteractive]);
        assert!(!info.is_verified_password_only());
    }
}

#[tokio::test]
#[ignore = "requires THINKTERM_AUTH_TEST_PYTHON with Paramiko; see docs/thinkterm/connect-reverse-access-implementation.md"]
async fn real_partial_public_key_success_cannot_qualify_as_password_only() {
    for backend in backends() {
        let (result, attempts) =
            exchange(backend, Scenario::PublicKeyThenPassword, true, false).await;
        let info = result.unwrap();
        assert!(attempts >= 2);
        assert!(!info.is_verified_password_only());
        if backend == AuthenticationBackend::LibSsh {
            assert!(info.is_complete());
            assert_eq!(
                info.methods(),
                &[
                    AuthenticationMethod::PublicKey,
                    AuthenticationMethod::Password
                ]
            );
        } else {
            assert!(!info.is_complete());
        }
    }
}

#[tokio::test]
async fn real_rejected_public_key_is_only_eligible_with_complete_evidence() {
    for backend in backends() {
        let (result, attempts) =
            exchange(backend, Scenario::RejectedKeyThenPassword, true, false).await;
        let info = result.unwrap();
        assert!(attempts >= 2);
        assert_eq!(
            info.is_verified_password_only(),
            backend == AuthenticationBackend::LibSsh
        );
    }
}

#[tokio::test]
#[ignore = "requires THINKTERM_AUTH_TEST_PYTHON with Paramiko; see docs/thinkterm/connect-reverse-access-implementation.md"]
async fn real_password_followed_by_another_factor_does_not_qualify() {
    for backend in backends() {
        let (result, attempts) =
            exchange(backend, Scenario::PasswordThenInteractive, true, false).await;
        let info = result.unwrap();
        assert_eq!(attempts, 2);
        assert!(!info.is_verified_password_only());
        if backend == AuthenticationBackend::LibSsh {
            assert_eq!(
                info.methods(),
                &[
                    AuthenticationMethod::Password,
                    AuthenticationMethod::KeyboardInteractive
                ]
            );
        } else {
            assert!(!info.is_complete());
        }
    }
}

#[tokio::test]
async fn declining_host_trust_never_attempts_credentials() {
    for backend in backends() {
        let (result, attempts) = exchange(backend, Scenario::Password, false, false).await;
        assert!(result.is_err());
        assert_eq!(attempts, 0);
    }
}

#[tokio::test]
async fn cancelling_password_produces_no_authentication_record() {
    for backend in backends() {
        let (result, attempts) = exchange(backend, Scenario::Password, true, true).await;
        assert!(result.is_err());
        assert_eq!(attempts, 0);
    }
}

#[cfg(feature = "libssh-rs")]
#[tokio::test]
async fn cancelling_key_decryption_does_not_fall_back_to_password() {
    let (result, attempts) = exchange(
        AuthenticationBackend::LibSsh,
        Scenario::EncryptedKey,
        true,
        true,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(attempts, 0);
}
