//! Authorization and bounded messages for a Connect reverse channel.
//!
//! The authority stays in the process that owns the terminal. Its handles
//! cannot be deserialized from a request. A route, grant ID, or configuration
//! value is never a substitute for an authentication transaction.

#![forbid(unsafe_code)]

mod authorization;
pub mod budget;
pub mod channel;
pub mod clock;
pub mod exchange;
pub mod ipc;
pub mod native;
pub mod protocol;
mod text;
pub mod wire;

pub use authorization::{
    Approval, Authority, Endpoint, GrantInfo, GrantRef, GrantScope, Id, ListPermit, Operation,
    Permission, Request, RequestPermit, Route, Target,
};

pub const MAX_ROUTES: usize = 16;
pub const MAX_GRANTS: usize = 64;
pub const MAX_TARGETS: usize = 256;
pub const MAX_TARGETS_PER_GRANT: usize = 32;
pub const MAX_READ_BYTES: usize = 1024 * 1024;
pub const MAX_READ_LINES: usize = 2000;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_KEYS: usize = 64;
pub const MAX_DURATION_SECS: u64 = 120 * 60;
pub const DEFAULT_DURATION_SECS: u64 = 30 * 60;
pub const AUTH_TIMEOUT_SECS: u64 = 120;
pub const REQUEST_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("authorization or target is unavailable")]
    Unavailable,
    #[error("authorization is no longer valid")]
    Revoked,
    #[error("authentication transaction is no longer valid")]
    StaleApproval,
    #[error("authentication was cancelled or refused")]
    Cancelled,
    #[error("the operation timed out")]
    Timeout,
    #[error("the request exceeds the operation limit")]
    TooLarge,
    #[error("the Connect request limit is reached")]
    Busy,
    #[error("the request was already handled")]
    Replay,
    #[error("invalid Connect message")]
    Protocol,
    #[error("invalid authorization duration")]
    Duration,
    #[error("the authorization clock is unavailable")]
    Clock,
    #[error("secure randomness is unavailable")]
    Random,
}

pub type Result<T> = std::result::Result<T, Error>;
