use parking_lot::{Condvar, Mutex, MutexGuard};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_CAPACITY: usize = 1024 * 1024;
const IDLE_RELEASE: Duration = Duration::from_millis(100);
/// One PTY read. A parser that keeps the queue this short is keeping up.
const KEPT_CAPACITY: usize = 64 * 1024;

struct State {
    bytes: VecDeque<u8>,
    /// When the parser last found more queued than `KEPT_CAPACITY`.
    last_backlog: Instant,
    writer_open: bool,
    reader_open: bool,
}

impl State {
    /// Storage left by a burst the parser finished a while ago. Output that
    /// never stops for `IDLE_RELEASE` would otherwise hold it indefinitely.
    fn outlived_burst(&mut self) -> bool {
        if self.bytes.len() > KEPT_CAPACITY {
            self.last_backlog = Instant::now();
            return false;
        }
        self.bytes.capacity() > KEPT_CAPACITY && self.last_backlog.elapsed() >= IDLE_RELEASE
    }
}

struct Shared {
    state: Mutex<State>,
    readable: Condvar,
    writable: Condvar,
    capacity: usize,
}

pub(crate) struct OutputSender(Arc<Shared>);
pub(crate) struct OutputReceiver(Arc<Shared>);

pub(crate) fn channel() -> (OutputSender, OutputReceiver) {
    channel_with_capacity(MAX_CAPACITY)
}

fn channel_with_capacity(capacity: usize) -> (OutputSender, OutputReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            bytes: VecDeque::new(),
            last_backlog: Instant::now(),
            writer_open: true,
            reader_open: true,
        }),
        readable: Condvar::new(),
        writable: Condvar::new(),
        capacity: capacity.clamp(1, MAX_CAPACITY),
    });
    (OutputSender(Arc::clone(&shared)), OutputReceiver(shared))
}

impl Write for OutputSender {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let shared = &self.0;
        let mut state = shared.state.lock();
        while state.bytes.len() == shared.capacity && state.reader_open {
            shared.writable.wait(&mut state);
        }
        if !state.reader_open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pane parser stopped",
            ));
        }
        let len = data.len().min(shared.capacity - state.bytes.len());
        let needed = state.bytes.len() + len;
        if needed > state.bytes.capacity() {
            // Grow only for queued output. The one-MiB limit is independent
            // of the parser batch size, as the socket buffer limit was.
            let capacity = needed.next_power_of_two().min(shared.capacity);
            let additional = capacity - state.bytes.len();
            state.bytes.reserve_exact(additional);
        }
        let was_empty = state.bytes.is_empty();
        state.bytes.extend(data[..len].iter().copied());
        drop(state);
        if was_empty {
            shared.readable.notify_one();
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for OutputReceiver {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let state = self.wait_for_data(|| {});
        Ok(self.copy_bytes(state, data))
    }
}

impl OutputReceiver {
    /// Allocate the parser's buffer only when output arrives. Release it
    /// with the byte ring after an idle wait, or once a burst is over and
    /// only sparse output follows; queued actions own their text.
    pub(crate) fn read_buffered(&mut self, data: &mut Vec<u8>, limit: usize) -> io::Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let mut state = self.wait_for_data(|| *data = Vec::new());
        if state.bytes.is_empty() {
            return Ok(0);
        }
        if state.outlived_burst() {
            state.bytes.shrink_to_fit();
            *data = Vec::new();
        }
        let len = limit.min(state.bytes.len());
        if data.len() < len {
            data.resize(len, 0);
        }
        Ok(self.copy_bytes(state, &mut data[..len]))
    }

    fn wait_for_data(&self, mut release_buffer: impl FnMut()) -> MutexGuard<'_, State> {
        let shared = &self.0;
        let mut state = shared.state.lock();
        while state.bytes.is_empty() && state.writer_open {
            if shared
                .readable
                .wait_for(&mut state, IDLE_RELEASE)
                .timed_out()
                && state.bytes.is_empty()
            {
                // A busy pane keeps its buffers. An idle pane gives the
                // storage back before sleeping until its next output.
                state.bytes = VecDeque::new();
                release_buffer();
                while state.bytes.is_empty() && state.writer_open {
                    shared.readable.wait(&mut state);
                }
            }
        }
        state
    }

    fn copy_bytes(&self, mut state: MutexGuard<'_, State>, data: &mut [u8]) -> usize {
        let len = data.len().min(state.bytes.len());
        let (first, second) = state.bytes.as_slices();
        let first_len = len.min(first.len());
        data[..first_len].copy_from_slice(&first[..first_len]);
        data[first_len..len].copy_from_slice(&second[..len - first_len]);
        let was_full = state.bytes.len() == self.0.capacity;
        state.bytes.drain(..len);
        drop(state);
        if was_full {
            self.0.writable.notify_one();
        }
        len
    }

    pub(crate) fn readable_now(&self) -> bool {
        let state = self.0.state.lock();
        !state.bytes.is_empty() || !state.writer_open
    }

    pub(crate) fn wait_readable(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock();
        while state.bytes.is_empty() && state.writer_open {
            if self.0.readable.wait_until(&mut state, deadline).timed_out() {
                break;
            }
        }
        !state.bytes.is_empty() || !state.writer_open
    }
}

impl Drop for OutputSender {
    fn drop(&mut self) {
        self.0.state.lock().writer_open = false;
        self.0.readable.notify_one();
    }
}

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        self.0.state.lock().reader_open = false;
        self.0.writable.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn output_keeps_order_across_wrap_partial_reads_and_eof() {
        let (mut tx, mut rx) = channel_with_capacity(127);
        let expected: Vec<u8> = (0..MAX_CAPACITY * 2 + 17).map(|n| n as u8).collect();
        let input = expected.clone();
        let writer = thread::spawn(move || {
            for bytes in input.chunks(43) {
                tx.write_all(bytes).unwrap();
            }
        });
        let mut received = Vec::new();
        let mut bytes = [0; 211];
        let mut limit = 1;
        loop {
            let len = rx.read(&mut bytes[..limit]).unwrap();
            if len == 0 {
                break;
            }
            received.extend_from_slice(&bytes[..len]);
            limit = (limit * 3) % bytes.len() + 1;
        }
        writer.join().unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn small_writes_are_coalesced_and_capacity_is_bounded() {
        let (mut tx, mut rx) = channel_with_capacity(20);
        tx.write_all(b"short ").unwrap();
        tx.write_all(b"command").unwrap();
        assert!(rx.readable_now());
        assert!(rx.wait_readable(Duration::ZERO));
        let mut bytes = [0; 32];
        assert_eq!(rx.read(&mut bytes).unwrap(), 13);
        assert_eq!(&bytes[..13], b"short command");
        assert_eq!(rx.0.state.lock().bytes.capacity(), 16);
        assert!(!rx.wait_readable(Duration::ZERO));
        drop(tx);
        assert!(rx.wait_readable(Duration::ZERO));
        assert_eq!(rx.read(&mut bytes).unwrap(), 0);
        assert_eq!(
            channel_with_capacity(usize::MAX).0 .0.capacity,
            MAX_CAPACITY
        );
        assert_eq!(channel_with_capacity(0).0 .0.capacity, 1);
    }

    #[test]
    fn blocked_writer_is_released_when_parser_closes() {
        let (mut tx, rx) = channel_with_capacity(1);
        tx.write_all(b"a").unwrap();
        let (done, result) = mpsc::channel();
        let writer = thread::spawn(move || done.send(tx.write_all(b"b")).unwrap());
        assert!(result.recv_timeout(Duration::from_millis(20)).is_err());
        drop(rx);
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        writer.join().unwrap();
    }

    #[test]
    fn idle_releases_storage_and_later_output_wakes_reader() {
        let (mut tx, mut rx) = channel_with_capacity(128);
        tx.write_all(b"a").unwrap();
        let mut byte = [0];
        assert_eq!(rx.read(&mut byte).unwrap(), 1);
        let shared = Arc::clone(&rx.0);
        let reader = thread::spawn(move || {
            assert_eq!(rx.read(&mut byte).unwrap(), 1);
            assert_eq!(&byte, b"b");
            assert_eq!(rx.read(&mut byte).unwrap(), 0);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.state.lock().bytes.capacity() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(shared.state.lock().bytes.capacity(), 0);
        tx.write_all(b"b").unwrap();
        drop(tx);
        reader.join().unwrap();
    }

    #[test]
    fn production_write_credit_is_one_mib_and_allocated_only_as_needed() {
        let (mut tx, mut rx) = channel();
        assert_eq!(tx.0.capacity, 1024 * 1024);
        assert_eq!(tx.0.state.lock().bytes.capacity(), 0);
        tx.write_all(b"x").unwrap();
        assert!(tx.0.state.lock().bytes.capacity() <= 8);
        let input = vec![b'y'; MAX_CAPACITY - 1];
        tx.write_all(&input).unwrap();
        assert_eq!(tx.0.state.lock().bytes.len(), MAX_CAPACITY);
        assert_eq!(tx.0.state.lock().bytes.capacity(), MAX_CAPACITY);
        let (started, ready) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let writer = thread::spawn(move || {
            started.send(()).unwrap();
            done.send(tx.write_all(b"z")).unwrap();
        });
        ready.recv().unwrap();
        assert!(result.recv_timeout(Duration::from_millis(20)).is_err());
        let mut head = [0; 1];
        assert_eq!(rx.read(&mut head).unwrap(), 1);
        assert_eq!(&head, b"x");
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        let mut rest = Vec::new();
        rx.read_to_end(&mut rest).unwrap();
        assert_eq!(&rest[..input.len()], input.as_slice());
        assert_eq!(rest.last(), Some(&b'z'));
    }

    #[test]
    fn growth_after_wrap_preserves_bytes_and_never_exceeds_the_bound() {
        let (mut tx, mut rx) = channel_with_capacity(1024);
        for initial in [3, 5, 31, 129] {
            let prefix: Vec<u8> = (0..initial).map(|n| n as u8).collect();
            tx.write_all(&prefix).unwrap();
            let mut removed = [0; 2];
            assert_eq!(rx.read(&mut removed).unwrap(), 2);
            assert_eq!(&removed, &prefix[..2]);
            let next: Vec<u8> = (0..initial * 2 + 7).map(|n| (n + 101) as u8).collect();
            tx.write_all(&next).unwrap();
            assert!(rx.0.state.lock().bytes.capacity() <= 1024);
            let mut actual = vec![0; prefix.len() - 2 + next.len()];
            rx.read_exact(&mut actual).unwrap();
            assert_eq!(actual, [&prefix[2..], next.as_slice()].concat());
        }
    }
    #[test]
    fn buffered_read_defers_allocation_until_data_and_preserves_read_limits() {
        let (tx, mut rx) = channel();
        let mut buffer = Vec::new();
        drop(tx);
        assert_eq!(rx.read_buffered(&mut buffer, 128 * 1024).unwrap(), 0);
        assert_eq!(buffer.capacity(), 0, "EOF needs no parser buffer");

        let (mut tx, mut rx) = channel_with_capacity(127);
        let expected: Vec<u8> = (0..127).collect();
        tx.write_all(&expected).unwrap();
        drop(tx);
        let mut actual = Vec::new();
        for limit in [7, 17, 3, 64, 128] {
            let len = rx.read_buffered(&mut buffer, limit).unwrap();
            assert!(len <= limit);
            actual.extend_from_slice(&buffer[..len]);
        }
        assert_eq!(actual, expected);
        assert_eq!(rx.read_buffered(&mut buffer, 128).unwrap(), 0);
        assert!(buffer.capacity() <= 128);
    }

    #[test]
    fn sparse_output_does_not_allocate_the_configured_batch_size() {
        let (mut tx, mut rx) = channel();
        let mut buffer = Vec::new();
        tx.write_all(b"ready").unwrap();
        assert_eq!(rx.read_buffered(&mut buffer, 128 * 1024).unwrap(), 5);
        assert_eq!(&buffer[..5], b"ready");
        assert!(buffer.capacity() <= 64);

        let expected = vec![b'x'; 128 * 1024 + 17];
        tx.write_all(&expected).unwrap();
        drop(tx);
        let mut received = Vec::new();
        loop {
            let len = rx.read_buffered(&mut buffer, 128 * 1024).unwrap();
            if len == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..len]);
        }
        assert_eq!(received, expected);
        assert!(buffer.capacity() <= 128 * 1024);
    }

    #[test]
    fn burst_storage_is_released_while_sparse_output_continues() {
        const BATCH: usize = 128 * 1024;
        let (mut tx, mut rx) = channel();
        let mut buffer = Vec::new();
        tx.write_all(&vec![b'x'; MAX_CAPACITY]).unwrap();
        let mut drained = 0;
        while drained < MAX_CAPACITY {
            drained += rx.read_buffered(&mut buffer, BATCH).unwrap();
        }
        assert_eq!(rx.0.state.lock().bytes.capacity(), MAX_CAPACITY);
        assert!(buffer.capacity() >= BATCH);

        // Straight after the burst the pane still counts as busy.
        rx.0.state.lock().last_backlog = Instant::now() + Duration::from_secs(60);
        tx.write_all(b"tick").unwrap();
        assert_eq!(rx.read_buffered(&mut buffer, BATCH).unwrap(), 4);
        assert_eq!(rx.0.state.lock().bytes.capacity(), MAX_CAPACITY);
        assert!(buffer.capacity() >= BATCH);

        // Output never stopped, so no idle wait ran, but the burst is over.
        // Queued bytes survive, including those a short read leaves behind.
        rx.0.state.lock().last_backlog = Instant::now() - IDLE_RELEASE;
        tx.write_all(b"tock").unwrap();
        assert_eq!(rx.read_buffered(&mut buffer, 2).unwrap(), 2);
        assert_eq!(&buffer[..2], b"to");
        assert!(buffer.capacity() <= 8);
        assert_eq!(rx.0.state.lock().bytes.capacity(), 4);
        assert_eq!(rx.read_buffered(&mut buffer, BATCH).unwrap(), 2);
        assert_eq!(&buffer[..2], b"ck");
    }

    #[test]
    fn a_deep_queue_counts_as_a_burst_however_old_the_last_one() {
        let (mut tx, mut rx) = channel();
        let mut buffer = Vec::new();
        tx.write_all(&vec![b'x'; KEPT_CAPACITY + 1]).unwrap();
        let capacity = rx.0.state.lock().bytes.capacity();
        assert!(capacity > KEPT_CAPACITY);
        let stale = Instant::now() - IDLE_RELEASE;
        rx.0.state.lock().last_backlog = stale;
        assert_eq!(rx.read_buffered(&mut buffer, 1).unwrap(), 1);
        assert_eq!(rx.0.state.lock().bytes.capacity(), capacity);
        assert!(rx.0.state.lock().last_backlog > stale);
    }

    #[test]
    fn one_pty_read_of_storage_stays_for_the_next_output() {
        let (mut tx, mut rx) = channel();
        let mut buffer = Vec::new();
        tx.write_all(&vec![b'x'; KEPT_CAPACITY]).unwrap();
        assert_eq!(
            rx.read_buffered(&mut buffer, KEPT_CAPACITY).unwrap(),
            KEPT_CAPACITY
        );
        rx.0.state.lock().last_backlog = Instant::now() - IDLE_RELEASE;
        tx.write_all(b"tick").unwrap();
        assert_eq!(rx.read_buffered(&mut buffer, KEPT_CAPACITY).unwrap(), 4);
        assert_eq!(rx.0.state.lock().bytes.capacity(), KEPT_CAPACITY);
        assert_eq!(buffer.len(), KEPT_CAPACITY);
    }

    #[test]
    fn idle_eof_releases_the_parser_buffer_and_byte_ring() {
        let (mut tx, mut rx) = channel_with_capacity(128);
        tx.write_all(b"ready").unwrap();
        let shared = Arc::clone(&rx.0);
        let (primed, ready) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buffer = Vec::new();
            assert_eq!(rx.read_buffered(&mut buffer, 64).unwrap(), 5);
            assert_eq!(&buffer[..5], b"ready");
            assert!(buffer.capacity() > 0 && buffer.capacity() <= 64);
            primed.send(()).unwrap();
            assert_eq!(rx.read_buffered(&mut buffer, 64).unwrap(), 0);
            assert_eq!(buffer.capacity(), 0);
        });
        ready.recv().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.state.lock().bytes.capacity() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(shared.state.lock().bytes.capacity(), 0);
        drop(tx);
        reader.join().unwrap();
    }
}
