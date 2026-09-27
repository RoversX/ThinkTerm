//! Telling that a file changed without reading it: its length and the time
//! it was last written. What the host keeps (the snippets, the switches)
//! and what it runs (manifests, programs) are compared by stamp before use,
//! so a change made from outside is picked up without watching for it.

use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// The file's stamp; `None` when there is no such file.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}
