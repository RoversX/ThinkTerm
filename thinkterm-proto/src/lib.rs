//! Pure-data types shared by the mux protocol (codec), the mux server and
//! every client. Moved here verbatim from `mux` and `config`, which
//! re-export them from their original paths, so the rest of the tree is
//! unaffected by the move.
//!
//! Two encodings depend on these definitions staying put, and codec's
//! `golden` test module pins both:
//! * the wire: varbincode is positional, so **field order** is the contract;
//! * saved Thread layouts: serde_json, so **field names** are the contract.
//!
//! The ids are `usize`. On the wire they travel as leb128 varints, so a
//! 32-bit peer (wasm) caps out at `u32::MAX` of them per session — accepted
//! deliberately; they are per-session counters and never get near that.

pub mod agent;
pub mod client;
pub mod command;
pub mod keyassignment;
pub mod layout;
pub mod pane;
pub mod renderable;
pub mod split;

pub use agent::{AgentEvidence, AgentState, AgentStatus};
pub use client::{ClientId, ClientInfo};
pub use command::{CommandSpec, EnvVar};
pub use keyassignment::{PaneDirection, ScrollbackEraseMode, SpawnTabDomain};
pub use layout::{PaneEntry, PaneNode, PaneStackEntry, SerdeUrl};
pub use pane::{Pattern, SearchResult};
pub use renderable::{RenderableDimensions, StableCursorPosition};
pub use split::{SplitDirection, SplitDirectionAndSize, SplitRequest, SplitSize};

pub type WindowId = usize;
pub type TabId = usize;
pub type PaneId = usize;
