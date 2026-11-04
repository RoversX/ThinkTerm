/// The backend that observed an SSH authentication exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationBackend {
    LibSsh,
    Ssh2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationMethod {
    None,
    PublicKey,
    Password,
    KeyboardInteractive,
}

const MAX_AUTHENTICATION_STEPS: usize = 16;

/// Read-only evidence from one session's handshake, without credentials or
/// prompt text. This is not user consent and does not establish liveness.
/// Consumers must use the record on the session that owns their channel.
#[derive(Debug)]
pub struct AuthenticationInfo {
    backend: AuthenticationBackend,
    methods: Vec<AuthenticationMethod>,
    complete: bool,
    host_verified: bool,
}

impl AuthenticationInfo {
    pub fn backend(&self) -> AuthenticationBackend {
        self.backend
    }

    /// Successful and partially successful steps, in observation order.
    /// If `is_complete` is false, other steps may be missing.
    pub fn methods(&self) -> &[AuthenticationMethod] {
        &self.methods
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn host_is_verified(&self) -> bool {
        self.host_verified
    }

    /// Method eligibility only: this must never create or extend a grant.
    pub fn is_verified_password_only(&self) -> bool {
        self.host_verified
            && self.complete
            && !self.methods.is_empty()
            && self
                .methods
                .iter()
                .all(|method| *method == AuthenticationMethod::Password)
    }
}

pub(crate) struct AuthenticationProgress {
    info: AuthenticationInfo,
}

impl AuthenticationProgress {
    pub(crate) fn new(backend: AuthenticationBackend) -> Self {
        Self {
            info: AuthenticationInfo {
                backend,
                methods: Vec::new(),
                complete: true,
                host_verified: false,
            },
        }
    }

    fn accepted(&mut self, method: AuthenticationMethod) {
        if self.info.methods.len() < MAX_AUTHENTICATION_STEPS {
            self.info.methods.push(method);
        } else {
            self.info.complete = false;
        }
    }

    #[cfg(feature = "libssh-rs")]
    pub(crate) fn observe_libssh(
        &mut self,
        method: AuthenticationMethod,
        status: libssh_rs::AuthStatus,
    ) {
        match status {
            libssh_rs::AuthStatus::Success | libssh_rs::AuthStatus::Partial => {
                self.accepted(method);
            }
            libssh_rs::AuthStatus::Denied
            | libssh_rs::AuthStatus::Info
            | libssh_rs::AuthStatus::Again => {}
        }
    }

    #[cfg(feature = "ssh2")]
    pub(crate) fn observe_ssh2(
        &mut self,
        method: AuthenticationMethod,
        succeeded: bool,
        authenticated: bool,
    ) {
        if succeeded && authenticated {
            self.accepted(method);
        } else {
            // libssh2 reports USERAUTH_FAILURE without exposing its partial
            // success flag. A rejected attempt cannot safely be discarded.
            self.info.complete = false;
        }
    }

    pub(crate) fn finish(mut self, host_verified: bool) -> AuthenticationInfo {
        self.info.host_verified = host_verified;
        if self.info.methods.is_empty() {
            self.info.complete = false;
        }
        self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_evidence_or_host_verification_never_qualifies() {
        for backend in [AuthenticationBackend::LibSsh, AuthenticationBackend::Ssh2] {
            assert!(!AuthenticationProgress::new(backend)
                .finish(true)
                .is_verified_password_only());
            let mut progress = AuthenticationProgress::new(backend);
            progress.accepted(AuthenticationMethod::Password);
            assert!(!progress.finish(false).is_verified_password_only());
        }
    }

    #[test]
    fn recording_is_bounded_and_overflow_cannot_qualify() {
        let mut progress = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
        for _ in 0..MAX_AUTHENTICATION_STEPS + 100 {
            progress.accepted(AuthenticationMethod::Password);
        }
        let info = progress.finish(true);
        assert_eq!(info.methods().len(), MAX_AUTHENTICATION_STEPS);
        assert!(!info.is_complete());
        assert!(!info.is_verified_password_only());
    }

    #[test]
    fn a_new_handshake_does_not_inherit_the_previous_chain() {
        let mut first = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
        first.accepted(AuthenticationMethod::Password);
        assert!(first.finish(true).is_verified_password_only());
        assert!(!AuthenticationProgress::new(AuthenticationBackend::LibSsh)
            .finish(true)
            .is_verified_password_only());
    }

    #[cfg(feature = "libssh-rs")]
    #[test]
    fn libssh_distinguishes_rejection_from_partial_public_key_success() {
        use libssh_rs::AuthStatus;
        for (status, eligible) in [(AuthStatus::Denied, true), (AuthStatus::Partial, false)] {
            let mut progress = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
            progress.observe_libssh(AuthenticationMethod::PublicKey, status);
            progress.observe_libssh(AuthenticationMethod::Password, AuthStatus::Success);
            let info = progress.finish(true);
            assert!(info.is_complete());
            assert_eq!(info.is_verified_password_only(), eligible);
        }
    }

    #[cfg(feature = "libssh-rs")]
    #[test]
    fn libssh_interactive_prompts_do_not_turn_into_password_evidence() {
        use libssh_rs::AuthStatus;
        let mut progress = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
        progress.observe_libssh(AuthenticationMethod::KeyboardInteractive, AuthStatus::Info);
        progress.observe_libssh(
            AuthenticationMethod::KeyboardInteractive,
            AuthStatus::Partial,
        );
        progress.observe_libssh(AuthenticationMethod::Password, AuthStatus::Success);
        let info = progress.finish(true);
        assert_eq!(
            info.methods(),
            &[
                AuthenticationMethod::KeyboardInteractive,
                AuthenticationMethod::Password
            ]
        );
        assert!(!info.is_verified_password_only());
    }

    #[cfg(feature = "libssh-rs")]
    #[test]
    fn libssh_accepts_only_verified_password_chains() {
        use libssh_rs::AuthStatus;
        for method in [
            AuthenticationMethod::None,
            AuthenticationMethod::PublicKey,
            AuthenticationMethod::KeyboardInteractive,
            AuthenticationMethod::Password,
        ] {
            let mut progress = AuthenticationProgress::new(AuthenticationBackend::LibSsh);
            progress.observe_libssh(method, AuthStatus::Success);
            let info = progress.finish(true);
            assert_eq!(info.backend(), AuthenticationBackend::LibSsh);
            assert!(info.host_is_verified());
            assert_eq!(
                info.is_verified_password_only(),
                method == AuthenticationMethod::Password
            );
        }
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn ssh2_ambiguous_prior_attempts_cannot_be_erased_by_password_success() {
        for method in [
            AuthenticationMethod::PublicKey,
            AuthenticationMethod::KeyboardInteractive,
            AuthenticationMethod::Password,
        ] {
            for (succeeded, authenticated) in [(false, false), (true, false), (false, true)] {
                let mut progress = AuthenticationProgress::new(AuthenticationBackend::Ssh2);
                progress.observe_ssh2(method, succeeded, authenticated);
                progress.observe_ssh2(AuthenticationMethod::Password, true, true);
                let info = progress.finish(true);
                assert!(!info.is_complete());
                assert!(!info.is_verified_password_only());
            }
        }
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn ssh2_accepts_only_observed_password_success() {
        for method in [
            AuthenticationMethod::None,
            AuthenticationMethod::PublicKey,
            AuthenticationMethod::KeyboardInteractive,
            AuthenticationMethod::Password,
        ] {
            let mut progress = AuthenticationProgress::new(AuthenticationBackend::Ssh2);
            progress.observe_ssh2(method, true, true);
            let info = progress.finish(true);
            assert_eq!(info.backend(), AuthenticationBackend::Ssh2);
            assert_eq!(
                info.is_verified_password_only(),
                method == AuthenticationMethod::Password
            );
        }
    }
}
