#[cfg(not(any(feature = "libssh-rs", feature = "ssh2")))]
compile_error!("Either libssh-rs or ssh2 must be enabled!");

mod addresses;
mod auth;
mod authentication;
#[cfg(all(test, unix))]
mod authentication_tests;
mod channelwrap;
mod config;
mod dirwrap;
mod filewrap;
mod host;
mod pty;
mod session;
mod sessioninner;
mod sessionwrap;
mod sftp;
mod sftpwrap;
mod through;

pub use addresses::OTHER_ADDRESSES;
pub use auth::*;
pub use authentication::{AuthenticationBackend, AuthenticationInfo, AuthenticationMethod};
pub use config::*;
pub use host::*;
pub use pty::*;
pub use session::*;
pub use sftp::error::*;
pub use sftp::types::*;
pub use sftp::*;
pub use through::{dial_registered, set_dialer, AddressFamily, Dialer, Network, NETWORK};

// NOTE: Re-exported as is exposed in a public API of this crate
pub use camino::{Utf8Path, Utf8PathBuf};
pub use filedescriptor::FileDescriptor;
pub use portable_pty::{Child, ChildKiller, MasterPty, PtySize};
