//! Reading a file from the host that owns a pane.
//!
//! A terminal link can name a path that exists on the machine running the
//! program, not on the machine drawing the window. The client asks that
//! machine for the bytes in bounded chunks and opens a local copy, so
//! "open with another app" works across a mux or SSH connection instead of
//! handing a foreign path to the local opener.
use crate::PaneId;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ReadRemoteFile {
    pub pane_id: PaneId,
    pub path: String,
    pub offset: u64,
    pub length: u32,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ReadRemoteFileResponse {
    pub pane_id: PaneId,
    pub data: Vec<u8>,
    /// No more bytes follow `offset + data.len()`.
    pub eof: bool,
}
