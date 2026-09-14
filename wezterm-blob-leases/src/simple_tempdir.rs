#![cfg(feature = "simple_tempdir")]

use crate::{BlobStorage, BoxedReader, BufSeekRead, ContentId, Error, LeaseId};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

pub struct SimpleTempDir {
    root: TempDir,
    refs: Mutex<HashMap<ContentId, usize>>,
}

impl SimpleTempDir {
    pub fn new() -> Result<Self, Error> {
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir()?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    pub fn new_in<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir_in(path)?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    fn path_for_content(&self, content_id: ContentId) -> Result<PathBuf, Error> {
        let path = self.root.path().join(format!("{content_id}"));
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|err| Error::StorageDirIoError(path.clone(), err))?;
        Ok(path)
    }

    fn add_ref(&self, content_id: ContentId) {
        *self.refs.lock().unwrap().entry(content_id).or_insert(0) += 1;
    }

    fn del_ref(&self, content_id: ContentId) {
        let mut refs = self.refs.lock().unwrap();
        match refs.get_mut(&content_id) {
            Some(count) if *count == 1 => {
                if let Ok(path) = self.path_for_content(content_id) {
                    if let Err(err) = std::fs::remove_file(&path) {
                        eprintln!("Failed to remove {}: {err:#}", path.display());
                    }
                }
                // No lease or reader remains. Keep deletion and removal
                // under the same lock as store(), but do not retain a
                // zero-count entry for every image ever seen.
                refs.remove(&content_id);
            }
            Some(count) => {
                *count -= 1;
            }
            None => {
                // Shouldn't really happen...
            }
        }
    }
}

impl BlobStorage for SimpleTempDir {
    fn store(&self, content_id: ContentId, data: &[u8], _lease_id: LeaseId) -> Result<(), Error> {
        let mut refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        let mut file = tempfile::Builder::new()
            .prefix("new-")
            .rand_bytes(5)
            .tempfile_in(&self.root.path())?;

        file.write_all(data)?;
        file.persist(&path)
            .map_err(|persist_err| persist_err.error)?;

        *refs.entry(content_id).or_insert(0) += 1;

        Ok(())
    }

    fn lease_by_content(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<(), Error> {
        let mut refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        if path.exists() {
            // Keep existence checking and acquiring the reference atomic with
            // del_ref(), without locking refs a second time via add_ref().
            *refs.entry(content_id).or_insert(0) += 1;
            Ok(())
        } else {
            Err(Error::ContentNotFound(content_id))
        }
    }

    fn get_data(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<Vec<u8>, Error> {
        let _refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        Ok(std::fs::read(&path).map_err(|err| Error::StorageDirIoError(path, err))?)
    }

    fn get_reader(&self, content_id: ContentId, lease_id: LeaseId) -> Result<BoxedReader, Error> {
        struct Reader {
            file: BufReader<File>,
            content_id: ContentId,
            lease_id: LeaseId,
        }

        impl BufSeekRead for Reader {}

        impl std::io::BufRead for Reader {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                self.file.fill_buf()
            }
            fn consume(&mut self, amount: usize) {
                self.file.consume(amount)
            }
        }

        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.file.read(buf)
            }
        }

        impl std::io::Seek for Reader {
            fn seek(&mut self, whence: std::io::SeekFrom) -> std::io::Result<u64> {
                self.file.seek(whence)
            }
        }

        impl Drop for Reader {
            fn drop(&mut self) {
                if let Ok(s) = crate::get_storage() {
                    s.advise_lease_dropped(self.lease_id, self.content_id).ok();
                }
            }
        }

        let path = self.path_for_content(content_id)?;
        let file = BufReader::new(std::fs::File::open(&path)?);
        self.add_ref(content_id);

        Ok(Box::new(Reader {
            file,
            content_id,
            lease_id,
        }))
    }

    fn advise_lease_dropped(&self, _lease_id: LeaseId, content_id: ContentId) -> Result<(), Error> {
        self.del_ref(content_id);
        Ok(())
    }

    fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }

    fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leasing_existing_content_completes_and_retains_it() {
        let storage = std::sync::Arc::new(SimpleTempDir::new().unwrap());
        let data = b"leased image";
        let id = ContentId::for_bytes(data);
        let first = LeaseId::new();
        let second = LeaseId::new();
        storage.store(id, data, first).unwrap();

        // A recursive refs lock must fail the test rather than hang the suite.
        let (send, recv) = std::sync::mpsc::channel();
        let worker_storage = std::sync::Arc::clone(&storage);
        let worker = std::thread::spawn(move || {
            send.send(worker_storage.lease_by_content(id, second))
                .unwrap();
        });
        recv.recv_timeout(std::time::Duration::from_secs(5))
            .expect("lease_by_content did not complete")
            .unwrap();
        worker.join().unwrap();

        assert_eq!(storage.refs.lock().unwrap().get(&id), Some(&2));
        storage.advise_lease_dropped(first, id).unwrap();
        assert_eq!(storage.get_data(id, second).unwrap(), data);
        storage.advise_lease_dropped(second, id).unwrap();
        assert!(!storage.path_for_content(id).unwrap().exists());
        assert!(storage.refs.lock().unwrap().is_empty());
    }

    #[test]
    fn leasing_missing_content_does_not_create_a_reference() {
        let storage = SimpleTempDir::new().unwrap();
        let id = ContentId::for_bytes(b"missing image");
        assert!(matches!(
            storage.lease_by_content(id, LeaseId::new()),
            Err(Error::ContentNotFound(missing)) if missing == id
        ));
        assert!(storage.refs.lock().unwrap().is_empty());
    }

    #[test]
    fn released_content_does_not_accumulate_reference_entries() {
        let storage = SimpleTempDir::new().unwrap();
        for value in 0..256u32 {
            let data = value.to_le_bytes();
            let id = ContentId::for_bytes(&data);
            let lease = LeaseId::new();
            storage.store(id, &data, lease).unwrap();
            storage.advise_lease_dropped(lease, id).unwrap();
            assert!(!storage.path_for_content(id).unwrap().exists());
            assert!(storage.refs.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn content_is_retained_until_its_last_reference() {
        let storage = SimpleTempDir::new().unwrap();
        let data = b"shared image";
        let id = ContentId::for_bytes(data);
        let first = LeaseId::new();
        let second = LeaseId::new();
        storage.store(id, data, first).unwrap();
        storage.store(id, data, second).unwrap();
        storage.advise_lease_dropped(first, id).unwrap();
        assert_eq!(storage.refs.lock().unwrap().get(&id), Some(&1));
        assert_eq!(storage.get_data(id, second).unwrap(), data);
        storage.advise_lease_dropped(second, id).unwrap();
        assert!(!storage.path_for_content(id).unwrap().exists());
        assert!(storage.refs.lock().unwrap().is_empty());
    }

    #[test]
    fn released_content_can_be_stored_again() {
        let storage = SimpleTempDir::new().unwrap();
        let data = b"same image again";
        let id = ContentId::for_bytes(data);
        for _ in 0..2 {
            let lease = LeaseId::new();
            storage.store(id, data, lease).unwrap();
            assert_eq!(storage.refs.lock().unwrap().get(&id), Some(&1));
            assert_eq!(storage.get_data(id, lease).unwrap(), data);
            storage.advise_lease_dropped(lease, id).unwrap();
            assert!(storage.refs.lock().unwrap().is_empty());
        }
    }
}
