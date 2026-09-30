//! Bounded transport chunks exposed as a byte stream to the PDU decoder.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QueueFull {
    #[error("receive byte budget exceeded")]
    Bytes,
    #[error("receive chunk budget exceeded")]
    Chunks,
    #[error("received chunk length changed")]
    Length,
}

pub struct ByteQueue {
    chunks: VecDeque<Box<[u8]>>,
    offset: usize,
    unread: usize,
    retained: usize,
    max_bytes: usize,
    max_chunks: usize,
}

impl ByteQueue {
    pub fn new(max_bytes: usize, max_chunks: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            offset: 0,
            unread: 0,
            retained: 0,
            max_bytes,
            max_chunks,
        }
    }

    pub fn len(&self) -> usize {
        self.unread
    }

    pub fn is_empty(&self) -> bool {
        self.unread == 0
    }

    /// Check the announced length before copying browser-owned bytes into
    /// Rust. A partially consumed chunk still charges its entire allocation.
    pub fn push_with(
        &mut self,
        len: usize,
        copy: impl FnOnce() -> Vec<u8>,
    ) -> Result<(), QueueFull> {
        if len == 0 {
            return Ok(());
        }
        if len > self.max_bytes.saturating_sub(self.retained) {
            return Err(QueueFull::Bytes);
        }
        if self.chunks.len() >= self.max_chunks {
            return Err(QueueFull::Chunks);
        }
        let bytes = copy();
        if bytes.len() != len {
            return Err(QueueFull::Length);
        }
        self.chunks.push_back(bytes.into_boxed_slice());
        self.retained += len;
        self.unread += len;
        Ok(())
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut read = 0;
        while read < buf.len() {
            let Some(chunk) = self.chunks.front() else {
                break;
            };
            let count = (chunk.len() - self.offset).min(buf.len() - read);
            buf[read..read + count].copy_from_slice(&chunk[self.offset..self.offset + count]);
            self.offset += count;
            self.unread -= count;
            read += count;
            if self.offset == chunk.len() {
                self.retained -= chunk.len();
                self.chunks.pop_front();
                self.offset = 0;
            }
        }
        read
    }

    /// Retiring a connection releases both its byte buffers and queue storage.
    pub fn clear(&mut self) {
        self.chunks = VecDeque::new();
        self.offset = 0;
        self.unread = 0;
        self.retained = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_transport_and_reader_boundaries_preserve_every_byte() {
        let source: Vec<u8> = (0..1027).map(|i| (i % 251) as u8).collect();
        for chunk_size in [1, 3, 17, 256, 1027] {
            for read_size in [1, 2, 13, 1028] {
                let mut queue = ByteQueue::new(source.len(), source.len());
                for chunk in source.chunks(chunk_size) {
                    queue.push_with(chunk.len(), || chunk.to_vec()).unwrap();
                }
                assert_eq!(queue.read(&mut []), 0);
                let mut result = Vec::new();
                let mut buf = vec![0; read_size];
                while !queue.is_empty() {
                    let n = queue.read(&mut buf);
                    result.extend_from_slice(&buf[..n]);
                }
                assert_eq!(result, source);
                assert_eq!((queue.retained, queue.offset, queue.len()), (0, 0, 0));
                assert!(queue.chunks.is_empty());
            }
        }
    }

    #[test]
    fn partially_read_allocations_stay_charged_until_released() {
        let mut queue = ByteQueue::new(8, 4);
        queue.push_with(8, || vec![7; 8]).unwrap();
        assert_eq!(queue.read(&mut [0; 7]), 7);
        assert_eq!((queue.len(), queue.retained), (1, 8));
        assert_eq!(
            queue.push_with(1, || panic!("must check before allocation")),
            Err(QueueFull::Bytes)
        );
        assert_eq!(queue.read(&mut [0; 1]), 1);
        queue.push_with(8, || vec![9; 8]).unwrap();
        let mut buf = [0; 8];
        assert_eq!(queue.read(&mut buf), 8);
        assert_eq!(buf, [9; 8]);
    }

    #[test]
    fn tiny_chunks_and_announced_oversize_cannot_bypass_limits() {
        let mut queue = ByteQueue::new(100, 2);
        queue.push_with(1, || vec![1]).unwrap();
        queue.push_with(1, || vec![2]).unwrap();
        assert_eq!(
            queue.push_with(1, || panic!("must check before allocation")),
            Err(QueueFull::Chunks)
        );
        queue
            .push_with(0, || panic!("empty input needs no allocation"))
            .unwrap();
        assert_eq!(
            queue.push_with(usize::MAX, || panic!("must not allocate")),
            Err(QueueFull::Bytes)
        );
        let mut buf = [0; 2];
        assert_eq!(queue.read(&mut buf), 2);
        assert_eq!(buf, [1, 2]);
    }

    #[test]
    fn retirement_releases_storage_and_length_mismatch_is_atomic() {
        let mut queue = ByteQueue::new(16, 4);
        queue.push_with(8, || vec![1; 8]).unwrap();
        queue.read(&mut [0; 3]);
        assert_eq!(queue.push_with(2, || vec![2; 3]), Err(QueueFull::Length));
        assert_eq!((queue.len(), queue.retained), (5, 8));
        queue.clear();
        assert_eq!(
            (
                queue.chunks.capacity(),
                queue.len(),
                queue.retained,
                queue.offset
            ),
            (0, 0, 0, 0)
        );
        queue.push_with(16, || vec![3; 16]).unwrap();
        assert_eq!(queue.read(&mut [0; 16]), 16);
    }
}
