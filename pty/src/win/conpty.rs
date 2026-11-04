use crate::cmdbuilder::CommandBuilder;
use crate::win::psuedocon::PsuedoCon;
use crate::{Child, MasterPty, PtyPair, PtySize, PtySystem, SlavePty};
use anyhow::Error;
use filedescriptor::{FileDescriptor, Pipe};
use std::convert::TryInto;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex};
use winapi::um::wincon::COORD;

/// A pseudo console as handles another process can adopt: what one mux
/// server hands to its successor for each pane. See
/// `MasterPty::conpty_handoff` and `master_from_handoff`.
pub struct ConPtyHandoff {
    /// The pipe the program's input is written to.
    pub input: OwnedHandle,
    /// The pipe the program's output is read from.
    pub output: OwnedHandle,
    /// The pseudo console: its signal pipe, its reference and the console
    /// host process.
    pub console: [OwnedHandle; 3],
}

impl ConPtyHandoff {
    /// How many handles `into_handles` yields and `from_handles` takes.
    pub const HANDLES: usize = 5;

    pub fn into_handles(self) -> Vec<OwnedHandle> {
        let [signal, reference, server] = self.console;
        vec![self.input, self.output, signal, reference, server]
    }

    pub fn from_handles(handles: Vec<OwnedHandle>) -> anyhow::Result<Self> {
        let count = handles.len();
        let mut handles = handles.into_iter();
        match (
            handles.next(),
            handles.next(),
            handles.next(),
            handles.next(),
            handles.next(),
            handles.next(),
        ) {
            (Some(input), Some(output), Some(signal), Some(reference), Some(server), None) => {
                Ok(Self {
                    input,
                    output,
                    console: [signal, reference, server],
                })
            }
            _ => anyhow::bail!(
                "a pseudo console is {} handles, not {count}",
                Self::HANDLES
            ),
        }
    }
}

/// Whether this process's pseudo consoles can be handed over at all: not
/// with the system's own, without the conpty.dll shipped alongside.
pub fn handoff_supported() -> bool {
    crate::win::psuedocon::handoff_supported()
}

/// The pty of a pane a predecessor handed over, from its handles. `size` is
/// what the pane last knew; the console host keeps its own.
pub fn master_from_handoff(
    handoff: ConPtyHandoff,
    size: PtySize,
) -> anyhow::Result<Box<dyn MasterPty + Send>> {
    let ConPtyHandoff {
        input,
        output,
        console,
    } = handoff;
    let con = PsuedoCon::from_handoff(console)?;
    Ok(Box::new(ConPtyMasterPty {
        inner: Arc::new(Mutex::new(Inner {
            con,
            readable: unsafe { FileDescriptor::from_raw_handle(output.into_raw_handle()) },
            writable: Some(unsafe { FileDescriptor::from_raw_handle(input.into_raw_handle()) }),
            input_for_handoff: None,
            delegated_input: None,
            size,
        })),
    }))
}

fn owned(fd: &FileDescriptor) -> anyhow::Result<OwnedHandle> {
    Ok(unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(fd.as_raw_handle()) }
        .try_clone_to_owned()?)
}

#[derive(Default)]
pub struct ConPtySystem {}

impl PtySystem for ConPtySystem {
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair> {
        let stdin = Pipe::new()?;
        let stdout = Pipe::new()?;

        let con = PsuedoCon::new(
            COORD {
                X: size.cols as i16,
                Y: size.rows as i16,
            },
            stdin.read,
            stdout.write,
        )?;

        let master = ConPtyMasterPty {
            inner: Arc::new(Mutex::new(Inner {
                con,
                readable: stdout.read,
                writable: Some(stdin.write),
                input_for_handoff: None,
                delegated_input: None,
                size,
            })),
        };

        let slave = ConPtySlavePty {
            inner: master.inner.clone(),
        };

        Ok(PtyPair {
            master: Box::new(master),
            slave: Box::new(slave),
        })
    }
}

struct Inner {
    con: PsuedoCon,
    readable: FileDescriptor,
    writable: Option<FileDescriptor>,
    /// A second handle on the input pipe once the writer is taken, for a
    /// handoff: the writer itself goes where this side cannot reach it.
    input_for_handoff: Option<FileDescriptor>,
    delegated_input: Option<Arc<NonblockingInput>>,
    size: PtySize,
}

impl Inner {
    pub fn resize(
        &mut self,
        num_rows: u16,
        num_cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), Error> {
        self.con.resize(COORD {
            X: num_cols as i16,
            Y: num_rows as i16,
        })?;
        self.size = PtySize {
            rows: num_rows,
            cols: num_cols,
            pixel_width,
            pixel_height,
        };
        Ok(())
    }
}

#[derive(Clone)]
pub struct ConPtyMasterPty {
    inner: Arc<Mutex<Inner>>,
}

pub struct ConPtySlavePty {
    inner: Arc<Mutex<Inner>>,
}

impl MasterPty for ConPtyMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.resize(size.rows, size.cols, size.pixel_width, size.pixel_height)
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.size.clone())
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(self.inner.lock().unwrap().readable.try_clone()?))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        let mut inner = self.inner.lock().unwrap();
        let writer = inner
            .writable
            .take()
            .ok_or_else(|| anyhow::anyhow!("writer already taken"))?;
        inner.input_for_handoff = writer.try_clone().ok();
        Ok(Box::new(ConPtyWriter(writer)))
    }

    fn try_input_writer(&self) -> anyhow::Result<Box<dyn crate::TryInputWriter>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("pty is unavailable"))?;
        let input = if let Some(input) = inner.delegated_input.as_ref() {
            Arc::clone(input)
        } else {
            let fd = inner
                .writable
                .as_ref()
                .or(inner.input_for_handoff.as_ref())
                .ok_or_else(|| anyhow::anyhow!("pty input is unavailable"))?
                .try_clone()?;
            set_pipe_nonblocking(&fd, true)?;
            let input = Arc::new(NonblockingInput {
                fd: Mutex::new(fd),
                closed: std::sync::atomic::AtomicBool::new(false),
            });
            inner.delegated_input = Some(Arc::clone(&input));
            input
        };
        Ok(Box::new(ConPtyTryInput(input)))
    }

    fn quiesce_input(&self) -> anyhow::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("pty input is unavailable"))?;
        if let Some(input) = inner.delegated_input.take() {
            let fd = input
                .fd
                .lock()
                .map_err(|_| anyhow::anyhow!("pty input is unavailable"))?;
            input
                .closed
                .store(true, std::sync::atomic::Ordering::Release);
            set_pipe_nonblocking(&fd, false)?;
        }
        Ok(())
    }

    fn conpty_handoff(&self) -> anyhow::Result<ConPtyHandoff> {
        let inner = self.inner.lock().unwrap();
        let input = inner
            .writable
            .as_ref()
            .or(inner.input_for_handoff.as_ref())
            .ok_or_else(|| anyhow::anyhow!("the pty's input pipe is gone"))?;
        Ok(ConPtyHandoff {
            input: owned(input)?,
            output: owned(&inner.readable)?,
            console: inner.con.handoff_handles()?,
        })
    }
}

fn set_pipe_nonblocking(fd: &FileDescriptor, enabled: bool) -> anyhow::Result<()> {
    use interprocess::os::windows::named_pipe::{pipe_mode::Bytes, SendPipeStream};
    let handle: OwnedHandle = fd.as_file()?.into();
    let pipe: SendPipeStream<Bytes> = handle
        .try_into()
        .map_err(|_| anyhow::anyhow!("pty input is not a pipe"))?;
    let result = pipe.set_nonblocking(enabled);
    // No flushing Drop path: this duplicate is used only to set pipe mode.
    let handle: OwnedHandle = pipe
        .try_into()
        .map_err(|_| anyhow::anyhow!("pty input handle is shared"))?;
    drop(handle);
    result?;
    Ok(())
}

struct NonblockingInput {
    fd: Mutex<FileDescriptor>,
    closed: std::sync::atomic::AtomicBool,
}

struct ConPtyTryInput(Arc<NonblockingInput>);

impl crate::TryInputWriter for ConPtyTryInput {
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut fd = self
            .0
            .fd
            .try_lock()
            .map_err(|_| io::ErrorKind::WouldBlock)?;
        if self.0.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        match fd.write(bytes) {
            Ok(0) if !bytes.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
            result => result,
        }
    }
}

struct ConPtyWriter(FileDescriptor);

impl Write for ConPtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            match self.0.write(bytes) {
                Ok(0) if !bytes.is_empty() => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl SlavePty for ConPtySlavePty {
    fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<Box<dyn Child + Send + Sync>> {
        let inner = self.inner.lock().unwrap();
        let child = inner.con.spawn_command(cmd)?;
        Ok(Box::new(child))
    }
}

#[cfg(test)]
mod delegated_input_tests {
    use super::*;
    use crate::TryInputWriter;
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn anonymous_pipe_backpressure_is_nonblocking_and_ordinary_input_still_works() {
        let pipe = Pipe::new().unwrap();
        let mut reader = pipe.read;
        let mut ordinary = ConPtyWriter(pipe.write);
        let fd = ordinary.0.try_clone().unwrap();
        set_pipe_nonblocking(&fd, true).unwrap();
        let (done, completed) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut writer = ConPtyTryInput(Arc::new(NonblockingInput {
                fd: Mutex::new(fd),
                closed: std::sync::atomic::AtomicBool::new(false),
            }));
            let mut written = 0;
            loop {
                match writer.try_write(&[b'x'; 4096]) {
                    Ok(count) if count > 0 => {
                        written += count;
                        assert!(
                            written <= 4 * 1024 * 1024,
                            "undrained pipe did not apply backpressure"
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    result => panic!("unexpected pipe write result: {:?}", result),
                }
            }
            done.send(written).unwrap();
        });
        let written = match completed.recv_timeout(Duration::from_secs(5)) {
            Ok(written) => written,
            Err(error) => {
                // Closing the reader also releases an incorrectly blocking
                // writer, so a failed test cannot strand a pipe worker.
                drop(reader);
                let _ = worker.join();
                panic!("delegated pipe write blocked: {}", error);
            }
        };
        worker.join().unwrap();
        assert!(written > 0);
        let mut bytes = vec![0; written];
        reader.read_exact(&mut bytes).unwrap();
        assert!(bytes.iter().all(|byte| *byte == b'x'));
        ordinary.write_all(b"ordinary").unwrap();
        let mut bytes = [0; 8];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ordinary");
        set_pipe_nonblocking(&ordinary.0, false).unwrap();
    }
}
