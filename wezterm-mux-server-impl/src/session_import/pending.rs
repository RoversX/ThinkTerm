use anyhow::{ensure, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Completion {
    done: smol::channel::Receiver<()>,
    installed: AtomicBool,
}

lazy_static::lazy_static! {
    static ref PENDING: Mutex<HashMap<String, Arc<Completion>>> = Mutex::new(HashMap::new());
}

/// Registered before publishing tree rows, released on success or any early
/// return. Only the serialized importer registers workspaces; its validated
/// plan bounds their number by MAX_PANES.
pub(super) struct PendingImport {
    workspaces: Vec<String>,
    completion: Arc<Completion>,
    _done: smol::channel::Sender<()>,
}

impl PendingImport {
    pub(super) fn new(workspaces: Vec<String>) -> Self {
        let (done, receiver) = smol::channel::bounded(1);
        let completion = Arc::new(Completion {
            done: receiver,
            installed: AtomicBool::new(false),
        });
        let mut pending = PENDING.lock().unwrap();
        assert!(pending.len() + workspaces.len() <= thinkterm_import::MAX_PANES);
        for workspace in &workspaces {
            assert!(pending
                .insert(workspace.clone(), completion.clone())
                .is_none());
        }
        Self {
            workspaces,
            completion,
            _done: done,
        }
    }

    pub(super) fn finish(self) {
        self.completion.installed.store(true, Ordering::Release);
    }
}

impl Drop for PendingImport {
    fn drop(&mut self) {
        let mut pending = PENDING.lock().unwrap();
        for workspace in &self.workspaces {
            pending.remove(workspace);
        }
        // Dropping the sole sender wakes every waiter, including on failure.
    }
}

pub async fn wait_for_workspace(workspace: &str) -> Result<()> {
    let completion = PENDING.lock().unwrap().get(workspace).cloned();
    if let Some(completion) = completion {
        let _ = completion.done.recv().await;
        ensure!(
            completion.installed.load(Ordering::Acquire),
            "Session import did not finish; reopen the Space after checking the import result"
        );
    }
    Ok(())
}

pub(crate) fn ensure_not_pending(workspace: &str) -> Result<()> {
    ensure!(
        !PENDING.lock().unwrap().contains_key(workspace),
        "Session import is still in progress; open its Space after it completes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{pin_mut, poll};

    #[test]
    fn waits_for_installation_without_blocking_other_workspaces() {
        smol::block_on(async {
            let guard = PendingImport::new(vec!["import-wait-example".into()]);
            let first = wait_for_workspace("import-wait-example");
            let second = wait_for_workspace("import-wait-example");
            pin_mut!(first, second);
            assert!(poll!(&mut first).is_pending());
            assert!(poll!(&mut second).is_pending());
            assert!(ensure_not_pending("import-wait-example").is_err());
            wait_for_workspace("unrelated-example").await.unwrap();
            guard.finish();
            first.await.unwrap();
            second.await.unwrap();
            ensure_not_pending("import-wait-example").unwrap();
        });
    }

    #[test]
    fn failed_or_cancelled_import_releases_waiters_with_an_error() {
        smol::block_on(async {
            let guard = PendingImport::new(vec!["import-failed-example".into()]);
            let waiting = wait_for_workspace("import-failed-example");
            pin_mut!(waiting);
            assert!(poll!(&mut waiting).is_pending());
            drop(guard);
            assert!(waiting.await.is_err());
            ensure_not_pending("import-failed-example").unwrap();
        });
    }
}
