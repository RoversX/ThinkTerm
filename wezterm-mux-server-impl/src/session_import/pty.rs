use anyhow::{Context, Result};
use portable_pty::{MasterPty, PtySize};
use rustix::event::{poll, PollFd, PollFlags};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Mutex;

/// Duplicates are reserved before commit; dropping them only closes them.
/// Unlike UnixMasterWriter, these files never inject EOF when dropped.
#[derive(Debug)]
pub struct ImportedPty {
    master: OwnedFd,
    reader: Mutex<Option<File>>,
    writer: Mutex<Option<File>>,
}

struct ImportedWriter(File);

struct ImportedReader(File);

impl Read for ImportedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // Preserve the source's shared O_NONBLOCK flag, but keep
                    // Read blocking even if the mux could not allocate its
                    // wake pipe. Poll needs no additional descriptors.
                    let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
                    match poll(&mut fds, None) {
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(err) => return Err(err.into()),
                        Ok(_) => {
                            if fds[0].revents().contains(PollFlags::NVAL) {
                                return Err(io::Error::from_raw_os_error(libc::EBADF));
                            }
                            // Read again on HUP/ERR too: a hung-up PTY can
                            // still have output buffered before its EOF.
                        }
                    }
                }
                result => return result,
            }
        }
    }
}

impl Write for ImportedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The source's duplicates share O_NONBLOCK with the source PTY. Leave
        // that flag intact for rollback, but complete each queued write.
        let mut remaining = buf;
        while !remaining.is_empty() {
            match self.0.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => remaining = &remaining[count..],
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => loop {
                    let mut fds = [PollFd::new(&self.0, PollFlags::OUT)];
                    match poll(&mut fds, None) {
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(err) => return Err(err.into()),
                        Ok(_) => {
                            if fds[0]
                                .revents()
                                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
                            {
                                return Err(io::ErrorKind::BrokenPipe.into());
                            }
                            break;
                        }
                    }
                },
                Err(err) => return Err(err),
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl ImportedPty {
    pub fn prepare(master: OwnedFd) -> Result<Self> {
        let reader = File::from(master.try_clone()?);
        let writer = File::from(master.try_clone()?);
        rustix::termios::tcgetwinsize(&master)?;
        Ok(Self {
            master,
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(Some(writer)),
        })
    }
}

impl MasterPty for ImportedPty {
    fn resize(&self, size: PtySize) -> Result<()> {
        rustix::termios::tcsetwinsize(
            &self.master,
            rustix::termios::Winsize {
                ws_row: size.rows,
                ws_col: size.cols,
                ws_xpixel: size.pixel_width,
                ws_ypixel: size.pixel_height,
            },
        )?;
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize> {
        let size = rustix::termios::tcgetwinsize(&self.master)?;
        Ok(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        if let Some(reader) = self.reader.lock().unwrap().take() {
            return Ok(Box::new(ImportedReader(reader)));
        }
        Ok(Box::new(ImportedReader(File::from(
            self.master.try_clone()?,
        ))))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(ImportedWriter(
            self.writer
                .lock()
                .unwrap()
                .take()
                .context("Imported terminal writer was already taken")?,
        )))
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.master.as_raw_fd())
    }
    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        rustix::termios::tcgetpgrp(&self.master)
            .ok()
            .map(|pid| pid.as_raw_pid())
    }
    fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
        nix::sys::termios::tcgetattr(self.master.as_fd()).ok()
    }
}
