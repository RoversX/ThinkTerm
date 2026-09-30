//! A blob store for clients that have none of their own.
//!
//! A server keeps some encoded pictures as blob leases, and a lease crosses
//! the wire as its bytes, to be stored again on the receiving side. With no
//! store registered that failed ("Storage has not been initialized"), the
//! reply carrying the picture could not be decoded and the connection was
//! dropped, then dropped again for the same picture after every reconnect.
//! This store keeps the bytes in memory, each blob until its last lease goes.

use std::collections::HashMap;
use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use termwiz::image::{ImageData, ImageDataType};
use wezterm_blob_leases::{BlobStorage, BoxedReader, BufSeekRead, ContentId, Error, LeaseId};

static IN_MEMORY: AtomicBool = AtomicBool::new(false);

/// Register the in-memory store, unless this process already has a store.
/// Once: a second store registered over the first would orphan every lease
/// taken from it.
pub fn ensure_blob_storage() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if wezterm_blob_leases::get_storage().is_err()
            && wezterm_blob_leases::register_storage(Arc::new(MemoryBlobs::default())).is_ok()
        {
            IN_MEMORY.store(true, Ordering::Relaxed);
        }
    });
}

/// With the in-memory store a lease is bytes in memory like any other, but
/// counted as none (`ImageData::len`). Filed as its bytes instead, it counts
/// against the image store's budget and goes when the store lets it go; the
/// blob goes with the last lease.
pub(crate) fn owned(data: Arc<ImageData>) -> Arc<ImageData> {
    if !IN_MEMORY.load(Ordering::Relaxed) {
        return data;
    }
    let bytes = match &*data.data() {
        ImageDataType::EncodedLease(lease) => lease.get_data().ok(),
        _ => None,
    };
    let Some(bytes) = bytes else { return data };
    let owned = ImageData::with_data_and_hash(ImageDataType::EncodedFile(bytes), data.hash());
    owned.set_generation(data.generation());
    Arc::new(owned)
}

#[derive(Default)]
struct MemoryBlobs {
    blobs: Mutex<HashMap<ContentId, (Arc<[u8]>, Vec<LeaseId>)>>,
}

impl MemoryBlobs {
    fn bytes(&self, content_id: ContentId) -> Result<Arc<[u8]>, Error> {
        let blobs = self.blobs.lock().unwrap();
        let (data, _) = blobs.get(&content_id).ok_or(Error::ContentNotFound(content_id))?;
        Ok(Arc::clone(data))
    }
}

impl BlobStorage for MemoryBlobs {
    fn store(&self, content_id: ContentId, data: &[u8], lease_id: LeaseId) -> Result<(), Error> {
        let mut blobs = self.blobs.lock().unwrap();
        let (_, leases) = blobs.entry(content_id).or_insert_with(|| (Arc::from(data), Vec::new()));
        leases.push(lease_id);
        Ok(())
    }

    fn lease_by_content(&self, content_id: ContentId, lease_id: LeaseId) -> Result<(), Error> {
        let mut blobs = self.blobs.lock().unwrap();
        let (_, leases) = blobs.get_mut(&content_id).ok_or(Error::ContentNotFound(content_id))?;
        leases.push(lease_id);
        Ok(())
    }

    fn get_data(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<Vec<u8>, Error> {
        Ok(self.bytes(content_id)?.to_vec())
    }

    fn get_reader(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<BoxedReader, Error> {
        Ok(Box::new(Reader(Cursor::new(self.bytes(content_id)?))))
    }

    fn advise_lease_dropped(&self, lease_id: LeaseId, content_id: ContentId) -> Result<(), Error> {
        let mut blobs = self.blobs.lock().unwrap();
        if let Some((_, leases)) = blobs.get_mut(&content_id) {
            leases.retain(|held| *held != lease_id);
            if leases.is_empty() {
                blobs.remove(&content_id);
            }
        }
        Ok(())
    }

    fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }

    fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }
}

/// The reader holds its own reference, so the bytes outlive a lease dropped
/// while a decoder still reads them.
struct Reader(Cursor<Arc<[u8]>>);

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl BufRead for Reader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.0.consume(amount)
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl BufSeekRead for Reader {}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{GetImageCellResponse, Pdu};
    use wezterm_blob_leases::BlobManager;

    #[test]
    fn a_leased_picture_decodes_and_leaves_with_its_last_lease() {
        ensure_blob_storage();
        let bytes = b"not really a picture, only its bytes".to_vec();
        let lease = BlobManager::store(&bytes).unwrap();
        let content_id = lease.content_id();
        let reply = Pdu::GetImageCellResponse(GetImageCellResponse {
            pane_id: 1,
            data: Some(Arc::new(ImageData::with_data(ImageDataType::EncodedLease(lease)))),
            data_generation: 0,
            frames_from: 0,
        });
        let mut wire = vec![];
        reply.encode(&mut wire, 3).unwrap();
        let decoded = Pdu::decode(wire.as_slice()).unwrap();
        drop(reply);
        let Pdu::GetImageCellResponse(GetImageCellResponse { data: Some(data), .. }) = decoded.pdu else {
            panic!("the reply did not decode as a picture");
        };
        match &*data.data() {
            ImageDataType::EncodedLease(lease) => assert_eq!(lease.get_data().unwrap(), bytes),
            other => panic!("unexpected payload {other:?}"),
        }
        drop(data);
        assert!(BlobManager::get_by_content_id(content_id).is_err());
    }

    #[test]
    fn a_leased_picture_is_filed_as_counted_bytes() {
        ensure_blob_storage();
        let bytes = b"another picture's bytes".to_vec();
        let lease = BlobManager::store(&bytes).unwrap();
        let content_id = lease.content_id();
        let leased = Arc::new(ImageData::with_data(ImageDataType::EncodedLease(lease)));
        leased.set_generation(4);
        assert_eq!(leased.len(), 0);
        let filed = owned(Arc::clone(&leased));
        assert!(matches!(&*filed.data(), ImageDataType::EncodedFile(data) if *data == bytes));
        assert_eq!((filed.hash(), filed.generation(), filed.len()), (leased.hash(), 4, bytes.len()));
        drop(leased);
        assert!(BlobManager::get_by_content_id(content_id).is_err());
    }
}
