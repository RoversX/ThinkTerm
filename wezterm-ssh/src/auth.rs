use crate::authentication::{AuthenticationBackend, AuthenticationMethod, AuthenticationProgress};
use crate::session::SessionEvent;
use anyhow::Context;
use smol::channel::{bounded, Sender};

#[derive(Debug)]
pub struct AuthenticationPrompt {
    pub prompt: String,
    pub echo: bool,
}

#[derive(Debug)]
pub struct AuthenticationEvent {
    pub username: String,
    pub instructions: String,
    pub prompts: Vec<AuthenticationPrompt>,
    pub(crate) reply: Sender<Vec<String>>,
}

impl AuthenticationEvent {
    pub async fn answer(self, answers: Vec<String>) -> anyhow::Result<()> {
        Ok(self.reply.send(answers).await?)
    }

    pub fn try_answer(self, answers: Vec<String>) -> anyhow::Result<()> {
        Ok(self.reply.try_send(answers)?)
    }
}

impl crate::sessioninner::SessionInner {
    #[cfg(feature = "ssh2")]
    fn agent_auth(
        &mut self,
        sess: &ssh2::Session,
        user: &str,
        progress: &mut AuthenticationProgress,
    ) -> anyhow::Result<bool> {
        if let Some(only) = self.config.get("identitiesonly") {
            if only == "yes" {
                log::trace!("Skipping agent auth because identitiesonly=yes");
                return Ok(false);
            }
        }

        let mut agent = sess.agent()?;
        if agent.connect().is_err() {
            // If the agent is around, we can proceed with other methods
            return Ok(false);
        }

        agent.list_identities()?;
        let identities = agent.identities()?;
        for identity in identities {
            let result = agent.userauth(user, &identity);
            progress.observe_ssh2(
                AuthenticationMethod::PublicKey,
                result.is_ok(),
                sess.authenticated(),
            );
            if result.is_ok() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    #[cfg(feature = "ssh2")]
    fn pubkey_auth(
        &mut self,
        sess: &ssh2::Session,
        user: &str,
        host: &str,
        progress: &mut AuthenticationProgress,
    ) -> anyhow::Result<bool> {
        use std::path::{Path, PathBuf};

        if let Some(files) = self.config.get("identityfile") {
            for file in files.split_whitespace() {
                let pubkey: PathBuf = format!("{}.pub", file).into();
                let file = Path::new(file);

                if !file.exists() {
                    continue;
                }

                let pubkey = if pubkey.exists() {
                    Some(pubkey.as_ref())
                } else {
                    None
                };

                // We try with no passphrase first, in case the key is unencrypted
                let result = sess.userauth_pubkey_file(user, pubkey, &file, None);
                progress.observe_ssh2(
                    AuthenticationMethod::PublicKey,
                    result.is_ok(),
                    sess.authenticated(),
                );
                match result {
                    Ok(_) => {
                        log::info!("pubkey_file immediately ok for {}", file.display());
                        return Ok(true);
                    }
                    Err(_) => {
                        // Most likely cause of error is that we need a passphrase
                        // to decrypt the key, so let's prompt the user for one.
                        let (reply, answers) = bounded(1);
                        self.tx_event
                            .try_send(SessionEvent::Authenticate(AuthenticationEvent {
                                username: "".to_string(),
                                instructions: "".to_string(),
                                prompts: vec![AuthenticationPrompt {
                                    prompt: format!(
                                        "Passphrase to decrypt {} for {}@{}:\n> ",
                                        file.display(),
                                        user,
                                        host
                                    ),
                                    echo: false,
                                }],
                                reply,
                            }))
                            .context("sending Authenticate request to user")?;

                        let answers = smol::block_on(answers.recv())
                            .context("waiting for authentication answers from user")?;

                        if answers.is_empty() {
                            anyhow::bail!("user cancelled authentication");
                        }

                        let passphrase = &answers[0];

                        let result =
                            sess.userauth_pubkey_file(user, pubkey, &file, Some(passphrase));
                        progress.observe_ssh2(
                            AuthenticationMethod::PublicKey,
                            result.is_ok(),
                            sess.authenticated(),
                        );
                        match result {
                            Ok(_) => {
                                return Ok(true);
                            }
                            Err(err) => {
                                log::warn!("pubkey auth: {:#}", err);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    #[cfg(feature = "libssh-rs")]
    pub fn authenticate_libssh(
        &mut self,
        sess: &libssh_rs::Session,
    ) -> anyhow::Result<AuthenticationProgress> {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let tx = self.tx_event.clone();
        let mut progress = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
        let cancelled = Arc::new(AtomicBool::new(false));
        let callback_cancelled = Arc::clone(&cancelled);

        // Set the callback for pubkey auth
        sess.set_auth_callback(move |prompt, echo, _verify, identity| {
            let (reply, answers) = bounded(1);
            tx.try_send(SessionEvent::Authenticate(AuthenticationEvent {
                username: "".to_string(),
                instructions: "".to_string(),
                prompts: vec![AuthenticationPrompt {
                    prompt: match identity {
                        Some(ident) => format!("{} ({}): ", prompt, ident),
                        None => prompt.to_string(),
                    },
                    echo,
                }],
                reply,
            }))
            .map_err(|_| {
                callback_cancelled.store(true, Ordering::Relaxed);
                libssh_rs::Error::fatal("authentication prompt channel closed")
            })?;

            let answers = smol::block_on(answers.recv()).map_err(|_| {
                callback_cancelled.store(true, Ordering::Relaxed);
                libssh_rs::Error::fatal("authentication answer channel closed")
            })?;
            answers.into_iter().next().ok_or_else(|| {
                callback_cancelled.store(true, Ordering::Relaxed);
                libssh_rs::Error::fatal("user cancelled authentication")
            })
        });

        use libssh_rs::{AuthMethods, AuthStatus};
        let status = sess.userauth_none(None)?;
        progress.observe_libssh(AuthenticationMethod::None, status);
        match status {
            AuthStatus::Success => return Ok(progress),
            _ => {}
        }

        loop {
            let auth_methods = sess.userauth_list(None)?;
            let mut status_by_method = HashMap::new();

            if auth_methods.contains(AuthMethods::PUBLIC_KEY) {
                let result = sess.userauth_public_key_auto(None, None);
                if cancelled.load(Ordering::Relaxed) {
                    anyhow::bail!("user cancelled authentication");
                }
                let status = result?;
                progress.observe_libssh(AuthenticationMethod::PublicKey, status);
                match status {
                    AuthStatus::Success => return Ok(progress),
                    AuthStatus::Partial => continue,
                    status => {
                        status_by_method.insert(AuthMethods::PUBLIC_KEY, status);
                    }
                }
            }

            if auth_methods.contains(AuthMethods::INTERACTIVE) {
                loop {
                    let status = sess.userauth_keyboard_interactive(None, None)?;
                    progress.observe_libssh(AuthenticationMethod::KeyboardInteractive, status);
                    match status {
                        AuthStatus::Success => return Ok(progress),
                        AuthStatus::Info => {
                            let info = sess.userauth_keyboard_interactive_info()?;

                            let (reply, answers) = bounded(1);
                            self.tx_event
                                .try_send(SessionEvent::Authenticate(AuthenticationEvent {
                                    username: sess.get_user_name()?,
                                    instructions: info.instruction,
                                    prompts: info
                                        .prompts
                                        .into_iter()
                                        .map(|p| AuthenticationPrompt {
                                            prompt: p.prompt,
                                            echo: p.echo,
                                        })
                                        .collect(),
                                    reply,
                                }))
                                .context("sending Authenticate request to user")?;

                            let answers = smol::block_on(answers.recv())
                                .context("waiting for authentication answers from user")?;

                            sess.userauth_keyboard_interactive_set_answers(&answers)?;

                            continue;
                        }
                        AuthStatus::Denied => {
                            break;
                        }
                        AuthStatus::Partial => continue,
                        status => {
                            anyhow::bail!("interactive auth status: {:?}", status);
                        }
                    }
                }
            }

            if auth_methods.contains(AuthMethods::PASSWORD) {
                let (reply, answers) = bounded(1);
                self.tx_event
                    .try_send(SessionEvent::Authenticate(AuthenticationEvent {
                        username: "".to_string(),
                        instructions: "".to_string(),
                        prompts: vec![AuthenticationPrompt {
                            prompt: "Password: ".to_string(),
                            echo: false,
                        }],
                        reply,
                    }))
                    .context("sending Authenticate request to user")?;

                let answers = smol::block_on(answers.recv())
                    .context("waiting for authentication answers from user")?;
                let pw = answers
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("user cancelled authentication"))?;

                let status = sess.userauth_password(None, Some(&pw))?;
                progress.observe_libssh(AuthenticationMethod::Password, status);
                match status {
                    AuthStatus::Success => return Ok(progress),
                    AuthStatus::Partial => continue,
                    status => anyhow::bail!("password auth status: {:?}", status),
                }
            }

            anyhow::bail!(
                "unhandled auth case; methods={:?}, status={:?}",
                auth_methods,
                status_by_method
            );
        }
    }

    #[cfg(feature = "ssh2")]
    pub fn authenticate(
        &mut self,
        sess: &ssh2::Session,
        user: &str,
        host: &str,
    ) -> anyhow::Result<AuthenticationProgress> {
        use std::collections::HashSet;
        let mut progress = AuthenticationProgress::new(AuthenticationBackend::Ssh2);

        loop {
            if sess.authenticated() {
                return Ok(progress);
            }

            // Re-query the auth methods on each loop as a successful method
            // may unlock a new method on a subsequent iteration (eg: password
            // auth may then unlock 2fac)
            let methods: HashSet<&str> = sess.auth_methods(&user)?.split(',').collect();
            log::trace!("ssh auth methods: {:?}", methods);
            if sess.authenticated() {
                progress.observe_ssh2(AuthenticationMethod::None, true, true);
                return Ok(progress);
            }

            if !sess.authenticated() && methods.contains("publickey") {
                if self.agent_auth(sess, user, &mut progress)? {
                    continue;
                }

                if self.pubkey_auth(sess, user, host, &mut progress)? {
                    continue;
                }
            }

            if !sess.authenticated() && methods.contains("password") {
                let (reply, answers) = bounded(1);
                self.tx_event
                    .try_send(SessionEvent::Authenticate(AuthenticationEvent {
                        username: user.to_string(),
                        instructions: "".to_string(),
                        prompts: vec![AuthenticationPrompt {
                            prompt: format!("Password for {}@{}: ", user, host),
                            echo: false,
                        }],
                        reply,
                    }))
                    .context("sending Authenticate request to user")?;

                let answers = smol::block_on(answers.recv())
                    .context("waiting for authentication answers from user")?;

                if answers.is_empty() {
                    anyhow::bail!("user cancelled authentication");
                }

                let result = sess.userauth_password(user, &answers[0]);
                progress.observe_ssh2(
                    AuthenticationMethod::Password,
                    result.is_ok(),
                    sess.authenticated(),
                );
                if let Err(err) = result {
                    log::error!("while attempting password auth: {}", err);
                }
            }

            if !sess.authenticated() && methods.contains("keyboard-interactive") {
                struct Helper<'a> {
                    tx_event: &'a Sender<SessionEvent>,
                }

                impl<'a> ssh2::KeyboardInteractivePrompt for Helper<'a> {
                    fn prompt<'b>(
                        &mut self,
                        username: &str,
                        instructions: &str,
                        prompts: &[ssh2::Prompt<'b>],
                    ) -> Vec<String> {
                        let (reply, answers) = bounded(1);
                        if let Err(err) = self.tx_event.try_send(SessionEvent::Authenticate(
                            AuthenticationEvent {
                                username: username.to_string(),
                                instructions: instructions.to_string(),
                                prompts: prompts
                                    .iter()
                                    .map(|p| AuthenticationPrompt {
                                        prompt: p.text.to_string(),
                                        echo: p.echo,
                                    })
                                    .collect(),
                                reply,
                            },
                        )) {
                            log::error!("sending Authenticate request to user: {:#}", err);
                            return vec![];
                        }

                        match smol::block_on(answers.recv()) {
                            Err(err) => {
                                log::error!(
                                    "waiting for authentication answers from user: {:#}",
                                    err
                                );
                                return vec![];
                            }
                            Ok(answers) => answers,
                        }
                    }
                }

                let mut helper = Helper {
                    tx_event: &self.tx_event,
                };

                let result = sess.userauth_keyboard_interactive(user, &mut helper);
                progress.observe_ssh2(
                    AuthenticationMethod::KeyboardInteractive,
                    result.is_ok(),
                    sess.authenticated(),
                );
                if let Err(err) = result {
                    log::error!("while attempting keyboard-interactive auth: {}", err);
                }
            }
        }
    }
}
