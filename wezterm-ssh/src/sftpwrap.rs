use crate::dirwrap::DirWrap;
use crate::filewrap::FileWrap;
use crate::sftp::types::{Metadata, OpenOptions, RenameOptions};
use crate::sftp::SftpChannelResult;
use camino::{Utf8Path, Utf8PathBuf};

pub(crate) enum SftpWrap {
    #[cfg(feature = "ssh2")]
    Ssh2(ssh2::Sftp),

    #[cfg(feature = "libssh-rs")]
    LibSsh(libssh_rs::Sftp),
}

/// Narrow a Unix mode to the permission bits libssh takes.
///
/// This used to be `mode.try_into().unwrap()`, which panics — and it panics on
/// the SSH session thread, taking the whole connection down with no error the
/// caller can see. Masking instead means a nonsensical mode produces a
/// nonsensical permission rather than a dead session.
#[cfg(feature = "libssh-rs")]
fn sftp_mode_bits(mode: i32) -> u32 {
    (mode as u32) & 0o7777
}

#[cfg(feature = "ssh2")]
fn pathconv(path: std::path::PathBuf) -> SftpChannelResult<Utf8PathBuf> {
    use crate::sftp::SftpChannelError;
    use std::convert::TryFrom;
    Ok(Utf8PathBuf::try_from(path).map_err(|x| {
        SftpChannelError::from(std::io::Error::new(std::io::ErrorKind::InvalidData, x))
    })?)
}

impl SftpWrap {
    pub fn open(&self, filename: &Utf8Path, opts: OpenOptions) -> SftpChannelResult<FileWrap> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => {
                let flags: ssh2::OpenFlags = opts.into();
                let mode = opts.mode;
                let open_type: ssh2::OpenType = opts.ty.into();

                let file = sftp.open_mode(filename.as_std_path(), flags, mode, open_type)?;
                Ok(FileWrap::Ssh2(file))
            }

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => {
                use crate::sftp::types::WriteMode;
                use libc::{O_APPEND, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
                use libssh_rs::OpenFlags;
                // O_CREAT is what lets a write actually bring the file into
                // existence, and O_TRUNC is the documented meaning of the
                // non-appending write mode. Without them, opening a path that
                // is not already there simply fails, so `Sftp::create` could
                // never create anything.
                let accesstype = match (opts.write, opts.read) {
                    (Some(WriteMode::Append), true) => O_RDWR | O_APPEND | O_CREAT,
                    (Some(WriteMode::Append), false) => O_WRONLY | O_APPEND | O_CREAT,
                    (Some(WriteMode::Write), false) => O_WRONLY | O_CREAT | O_TRUNC,
                    (Some(WriteMode::Write), true) => O_RDWR | O_CREAT | O_TRUNC,
                    (Some(WriteMode::CreateNew), false) => O_WRONLY | O_CREAT | O_EXCL,
                    (Some(WriteMode::CreateNew), true) => O_RDWR | O_CREAT | O_EXCL,
                    (None, true) => O_RDONLY,
                    (None, false) => 0,
                };
                let file = sftp.open(
                    filename.as_str(),
                    OpenFlags::from_bits_truncate(accesstype),
                    sftp_mode_bits(opts.mode),
                )?;
                Ok(FileWrap::LibSsh(file))
            }
        }
    }

    pub fn symlink(&self, path: &Utf8Path, target: &Utf8Path) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.symlink(path.as_std_path(), target.as_std_path())?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.symlink(path.as_str(), target.as_str())?),
        }
    }

    pub fn read_link(&self, filename: &Utf8Path) -> SftpChannelResult<Utf8PathBuf> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(pathconv(sftp.readlink(filename.as_std_path())?)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.read_link(filename.as_str())?.into()),
        }
    }

    pub fn canonicalize(&self, filename: &Utf8Path) -> SftpChannelResult<Utf8PathBuf> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(pathconv(sftp.realpath(filename.as_std_path())?)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.canonicalize(filename.as_str())?.into()),
        }
    }

    pub fn unlink(&self, filename: &Utf8Path) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.unlink(filename.as_std_path())?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.remove_file(filename.as_str())?),
        }
    }

    pub fn remove_dir(&self, filename: &Utf8Path) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.rmdir(filename.as_std_path())?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.remove_dir(filename.as_str())?),
        }
    }

    pub fn create_dir(&self, filename: &Utf8Path, mode: i32) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.mkdir(filename.as_std_path(), mode)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.create_dir(filename.as_str(), sftp_mode_bits(mode))?),
        }
    }

    pub fn rename(
        &self,
        src: &Utf8Path,
        dest: &Utf8Path,
        #[cfg_attr(not(feature = "ssh2"), allow(unused_variables))] opts: RenameOptions,
    ) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => {
                Ok(sftp.rename(src.as_std_path(), dest.as_std_path(), Some(opts.into()))?)
            }

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.rename(src.as_str(), dest.as_str())?),
        }
    }

    pub fn symlink_metadata(&self, filename: &Utf8Path) -> SftpChannelResult<Metadata> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.lstat(filename.as_std_path()).map(Metadata::from)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp
                .symlink_metadata(filename.as_str())
                .map(Metadata::from)?),
        }
    }

    pub fn metadata(&self, filename: &Utf8Path) -> SftpChannelResult<Metadata> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.stat(filename.as_std_path()).map(Metadata::from)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.metadata(filename.as_str()).map(Metadata::from)?),
        }
    }

    pub fn set_metadata(&self, filename: &Utf8Path, metadata: Metadata) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.setstat(filename.as_std_path(), metadata.into())?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => {
                let attr: libssh_rs::SetAttributes = metadata.into();
                Ok(sftp.set_metadata(filename.as_str(), &attr)?)
            }
        }
    }

    pub fn open_dir(&self, filename: &Utf8Path) -> SftpChannelResult<DirWrap> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => Ok(sftp.opendir(filename.as_std_path()).map(DirWrap::Ssh2)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => Ok(sftp.open_dir(filename.as_str()).map(DirWrap::LibSsh)?),
        }
    }

    pub fn read_dir(&self, filename: &Utf8Path) -> SftpChannelResult<Vec<(Utf8PathBuf, Metadata)>> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sftp) => {
                let entries = sftp.readdir(filename.as_std_path())?;
                let mut mapped_entries = vec![];
                for (path, stat) in entries {
                    let path = pathconv(path)?;
                    mapped_entries.push((path, Metadata::from(stat)));
                }

                Ok(mapped_entries)
            }

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sftp) => {
                let entries = sftp.read_dir(filename.as_str())?;
                let mut mapped_entries = vec![];
                for metadata in entries {
                    let path = metadata
                        .name()
                        .expect("name to be present in read dir results");
                    if path == "." || path == ".." {
                        continue;
                    }
                    mapped_entries.push((filename.join(path), metadata.into()));
                }

                Ok(mapped_entries)
            }
        }
    }
}
