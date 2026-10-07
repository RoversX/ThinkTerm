use anyhow::{bail, ensure, Context, Result};
use codec::{ImportSessionRequest, ImportSessionResponse, ImportSessionStatus};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const RETENTION: u64 = 7 * 24 * 60 * 60;
const MAX_RECEIPTS: usize = 128;
const MAX_BYTES: usize = 32 * 1024 * 1024;

lazy_static::lazy_static! {
    static ref ACTIVE: Mutex<HashSet<(PathBuf, String)>> = Mutex::new(HashSet::new());
}

#[derive(Serialize, Deserialize)]
enum Outcome {
    Running,
    Completed,
    Failed(String),
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    request: ImportSessionRequest,
    outcome: Outcome,
    destination: Option<ImportSessionResponse>,
}

type Records = BTreeMap<String, Receipt>;

#[derive(Clone)]
pub(super) struct Store {
    path: PathBuf,
}

pub(super) enum Started {
    New(Attempt),
    Completed(ImportSessionResponse),
}

pub(super) struct Attempt {
    store: Store,
    request_id: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn issued_at(id: &str) -> Result<u64> {
    ensure!(id.len() <= 64, "Invalid import request ID");
    let (time, uuid) = id.split_once('-').context("Invalid import request ID")?;
    let timestamp: u64 = time.parse().context("Invalid import request timestamp")?;
    let parsed = uuid::Uuid::parse_str(uuid).context("Invalid import request UUID")?;
    ensure!(
        parsed.get_version_num() == 4
            && parsed.to_string() == uuid
            && timestamp.to_string() == time,
        "Invalid import request ID"
    );
    // The time comes from the client's clock. A server whose clock is behind
    // (a small board without NTP) still accepts it; the receipt starts to
    // age once this clock passes that time.
    Ok(timestamp)
}

impl Store {
    pub(super) fn owner() -> Self {
        Self {
            path: config::DATA_DIR.join("import-receipts.json"),
        }
    }

    fn locked<T>(&self, action: impl FnOnce(&mut Records) -> Result<(T, bool)>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .context("Import receipt directory is missing")?;
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        let mut records = match std::fs::File::open(&self.path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= MAX_BYTES,
                    "Import receipts exceed the size limit"
                );
                let records: Records =
                    serde_json::from_slice(&bytes).context("Read import receipts")?;
                ensure!(records.len() <= MAX_RECEIPTS, "Too many import receipts");
                records
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Records::new(),
            Err(err) => return Err(err.into()),
        };
        let (result, changed) = action(&mut records)?;
        if changed {
            let bytes = serde_json::to_vec(&records)?;
            ensure!(
                bytes.len() <= MAX_BYTES,
                "Import receipt storage is full; wait for older requests to expire"
            );
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            file.persist(&self.path)?;
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(result)
    }

    pub(super) fn status(&self, id: &str) -> Result<ImportSessionStatus> {
        if now().saturating_sub(issued_at(id)?) > RETENTION {
            return Ok(ImportSessionStatus::Expired);
        }
        self.locked(|records| {
            let status = match records.get(id) {
                None => ImportSessionStatus::NotFound,
                Some(receipt) => match &receipt.outcome {
                    Outcome::Completed => ImportSessionStatus::Completed(
                        receipt
                            .destination
                            .clone()
                            .context("Completed import receipt has no result")?,
                    ),
                    Outcome::Failed(error) => ImportSessionStatus::Failed(error.clone()),
                    Outcome::Running => {
                        if ACTIVE
                            .lock()
                            .unwrap()
                            .contains(&(self.path.clone(), id.to_owned()))
                        {
                            ImportSessionStatus::Running
                        } else {
                            ImportSessionStatus::Interrupted {
                                space_id: receipt
                                    .destination
                                    .as_ref()
                                    .map(|result| result.space_id.clone()),
                            }
                        }
                    }
                },
            };
            Ok((status, false))
        })
    }

    pub(super) fn start(&self, request: ImportSessionRequest) -> Result<Started> {
        let time = issued_at(&request.request_id)?;
        ensure!(
            now().saturating_sub(time) <= RETENTION,
            "Import request expired; inspect the session before starting a new import"
        );
        request.request.validate()?;
        self.locked(|records| {
            if let Some(receipt) = records.get(&request.request_id) {
                ensure!(receipt.request == request, "Import request ID was already used with different contents");
                return match &receipt.outcome {
                    Outcome::Completed => Ok((Started::Completed(receipt.destination.clone()
                        .context("Completed import receipt has no result")?), false)),
                    Outcome::Failed(error) => bail!("Previous import failed: {}", error),
                    Outcome::Running => bail!("Import was already accepted; query its result before taking further action"),
                };
            }
            records.retain(|id, _| issued_at(id).map_or(true, |time| now().saturating_sub(time) <= RETENTION));
            ensure!(records.len() < MAX_RECEIPTS, "Import receipt storage is full; wait for older requests to expire");
            let id = request.request_id.clone();
            records.insert(id.clone(), Receipt { request, outcome: Outcome::Running, destination: None });
            ensure!(serde_json::to_vec(records)?.len() <= MAX_BYTES - 16384,
                "Import receipt storage is full; wait for older requests to expire");
            Ok((Started::New(Attempt { store: self.clone(), request_id: id }), true))
        }).map(|started| {
            if let Started::New(attempt) = &started {
                ACTIVE.lock().unwrap().insert((self.path.clone(), attempt.request_id.clone()));
            }
            started
        })
    }
}

impl Store {
    fn set_destination(&self, id: &str, result: ImportSessionResponse) -> Result<()> {
        self.locked(|records| {
            records
                .get_mut(id)
                .context("Import receipt is missing")?
                .destination = Some(result);
            // Check the full result before touching the session and leave room
            // for its final outcome, including a bounded failure message.
            ensure!(
                serde_json::to_vec(records)?.len() <= MAX_BYTES - 16384,
                "Import receipt storage is full; wait for older requests to expire"
            );
            Ok(((), true))
        })
    }

    fn set_outcome(&self, id: &str, outcome: Outcome) -> Result<()> {
        self.locked(|records| {
            records
                .get_mut(id)
                .context("Import receipt is missing")?
                .outcome = outcome;
            Ok(((), true))
        })
    }
}

impl Attempt {
    pub(super) async fn destination(&self, result: ImportSessionResponse) -> Result<()> {
        self.write(move |store, id| store.set_destination(id, result))
            .await
    }

    pub(super) async fn complete(&self) -> Result<()> {
        self.write(|store, id| store.set_outcome(id, Outcome::Completed))
            .await
    }

    pub(super) async fn fail(&self, error: &str) -> Result<()> {
        let outcome = Outcome::Failed(error.chars().take(1024).collect());
        self.write(move |store, id| store.set_outcome(id, outcome))
            .await
    }

    /// Receipt writes lock a file and wait for the disk. They run on a
    /// blocking thread so the thread that serves terminals never waits.
    async fn write(
        &self,
        write: impl FnOnce(&Store, &str) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let store = self.store.clone();
        let id = self.request_id.clone();
        smol::unblock(move || write(&store, &id)).await
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        ACTIVE
            .lock()
            .unwrap()
            .remove(&(self.store.path.clone(), self.request_id.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(time: u64) -> ImportSessionRequest {
        ImportSessionRequest {
            request_id: format!("{}-{}", time, uuid::Uuid::new_v4()),
            request: thinkterm_import::ImportRequest {
                source: "example".into(),
                session: "development".into(),
                mode: thinkterm_import::ImportMode::Layout,
                fingerprint: "example-fingerprint".into(),
                space_name: "Imported".into(),
            },
        }
    }

    fn destination() -> ImportSessionResponse {
        ImportSessionResponse {
            tree: codec::ThinkTermTree::default(),
            space_id: "space-example".into(),
            workspace: "workspace-example".into(),
            live: false,
            pane_count: 1,
        }
    }

    fn begin(store: &Store, request: ImportSessionRequest) -> Attempt {
        match store.start(request).unwrap() {
            Started::New(attempt) => attempt,
            Started::Completed(_) => panic!("unexpected replay"),
        }
    }

    #[test]
    fn completed_result_survives_reload_and_replays_without_starting_an_import() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("receipts.json"),
        };
        let request = request(now());
        assert_eq!(
            store.status(&request.request_id).unwrap(),
            ImportSessionStatus::NotFound
        );
        let attempt = begin(&store, request.clone());
        assert_eq!(
            store.status(&request.request_id).unwrap(),
            ImportSessionStatus::Running
        );
        smol::block_on(attempt.destination(destination())).unwrap();
        smol::block_on(attempt.complete()).unwrap();
        drop(attempt);
        let reloaded = Store {
            path: store.path.clone(),
        };
        assert_eq!(
            reloaded.status(&request.request_id).unwrap(),
            ImportSessionStatus::Completed(destination())
        );
        assert!(
            matches!(reloaded.start(request.clone()).unwrap(), Started::Completed(result) if result == destination())
        );
        let mut changed = request;
        changed.request.session = "another-session".into();
        assert!(reloaded.start(changed).is_err());
    }

    #[test]
    fn interrupted_and_failed_attempts_are_never_executed_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("receipts.json"),
        };
        let interrupted = request(now());
        let attempt = begin(&store, interrupted.clone());
        smol::block_on(attempt.destination(destination())).unwrap();
        drop(attempt);
        assert_eq!(
            store.status(&interrupted.request_id).unwrap(),
            ImportSessionStatus::Interrupted {
                space_id: Some("space-example".into()),
            }
        );
        assert!(store.start(interrupted).is_err());
        let failed = request(now());
        let attempt = begin(&store, failed.clone());
        smol::block_on(attempt.fail("example prepare failure")).unwrap();
        drop(attempt);
        assert_eq!(
            store.status(&failed.request_id).unwrap(),
            ImportSessionStatus::Failed("example prepare failure".into())
        );
        assert!(store.start(failed).is_err());
    }

    #[test]
    fn expiry_frees_storage_without_allowing_old_requests_to_execute_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("receipts.json"),
        };
        let old = request(now() - RETENTION - 10);
        store
            .locked(|records| {
                records.insert(
                    old.request_id.clone(),
                    Receipt {
                        request: old.clone(),
                        outcome: Outcome::Completed,
                        destination: Some(destination()),
                    },
                );
                Ok(((), true))
            })
            .unwrap();
        assert_eq!(
            store.status(&old.request_id).unwrap(),
            ImportSessionStatus::Expired
        );
        assert!(store.start(old.clone()).is_err());
        let current = request(now());
        drop(begin(&store, current));
        store
            .locked(|records| {
                assert!(!records.contains_key(&old.request_id));
                Ok(((), false))
            })
            .unwrap();
        assert!(store.start(old).is_err());
    }

    #[test]
    fn a_server_clock_behind_the_client_still_accepts_requests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("receipts.json"),
        };
        let ahead = request(now() + 24 * 60 * 60);
        assert_eq!(
            store.status(&ahead.request_id).unwrap(),
            ImportSessionStatus::NotFound
        );
        let attempt = begin(&store, ahead.clone());
        assert_eq!(
            store.status(&ahead.request_id).unwrap(),
            ImportSessionStatus::Running
        );
        drop(attempt);
    }

    #[test]
    fn full_or_unreadable_receipts_refuse_new_imports() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            path: dir.path().join("receipts.json"),
        };
        store
            .locked(|records| {
                for _ in 0..MAX_RECEIPTS {
                    let request = request(now());
                    records.insert(
                        request.request_id.clone(),
                        Receipt {
                            request,
                            outcome: Outcome::Completed,
                            destination: Some(destination()),
                        },
                    );
                }
                Ok(((), true))
            })
            .unwrap();
        assert!(store.start(request(now())).is_err());
        std::fs::write(&store.path, b"invalid receipt data").unwrap();
        let unknown = request(now());
        assert!(store.status(&unknown.request_id).is_err());
        assert!(store.start(unknown).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), b"invalid receipt data");
    }
}
