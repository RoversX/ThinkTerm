use crate::channelwrap::ChannelWrap;
use crate::sftpwrap::SftpWrap;
use filedescriptor::{AsRawSocketDescriptor, SocketDescriptor, POLLIN, POLLOUT};

#[cfg(feature = "ssh2")]
pub(crate) struct Ssh2Session {
    pub sess: ssh2::Session,
    pub sftp: Option<SftpWrap>,
}

#[cfg(feature = "libssh-rs")]
pub(crate) struct LibSshSession {
    pub sess: libssh_rs::Session,
    pub sftp: Option<SftpWrap>,
}

pub(crate) enum SessionWrap {
    #[cfg(feature = "ssh2")]
    Ssh2(Ssh2Session),

    #[cfg(feature = "libssh-rs")]
    LibSsh(LibSshSession),
}

impl SessionWrap {
    #[cfg(feature = "ssh2")]
    pub fn with_ssh2(sess: ssh2::Session) -> Self {
        Self::Ssh2(Ssh2Session { sess, sftp: None })
    }

    #[cfg(feature = "libssh-rs")]
    pub fn with_libssh(sess: libssh_rs::Session) -> Self {
        Self::LibSsh(LibSshSession { sess, sftp: None })
    }

    pub fn set_blocking(&mut self, blocking: bool) {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sess) => sess.sess.set_blocking(blocking),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => sess.sess.set_blocking(blocking),
        }
    }

    pub fn get_poll_flags(&self) -> i16 {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sess) => match sess.sess.block_directions() {
                ssh2::BlockDirections::None => 0,
                ssh2::BlockDirections::Inbound => POLLIN,
                ssh2::BlockDirections::Outbound => POLLOUT,
                ssh2::BlockDirections::Both => POLLIN | POLLOUT,
            },

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => {
                let (read, write) = sess.sess.get_poll_state();
                match (read, write) {
                    (false, false) => 0,
                    (true, false) => POLLIN,
                    (false, true) => POLLOUT,
                    (true, true) => POLLIN | POLLOUT,
                }
            }
        }
    }

    /// Have the backend take in what its socket holds and see that it can
    /// be written to. Reading a channel does this on the way; this is for
    /// when no channel is there to read. False once the connection is found
    /// to have ended.
    pub fn service_socket(&mut self) -> bool {
        match self {
            // The socket left ready with nobody to take it up was seen
            // and measured with libssh alone.
            #[cfg(feature = "ssh2")]
            Self::Ssh2(_) => true,

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => {
                // libssh handles its socket only within a call made on a
                // channel. One that is never opened is nothing the far end
                // hears of, and is freed as it is dropped. The call fails
                // only when the session itself has.
                match sess.sess.new_channel() {
                    Ok(channel) => channel
                        .poll_timeout(false, Some(std::time::Duration::ZERO))
                        .is_ok(),
                    Err(_) => true,
                }
            }
        }
    }

    pub fn as_socket_descriptor(&self) -> SocketDescriptor {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sess) => sess.sess.as_socket_descriptor(),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => sess.sess.as_socket_descriptor(),
        }
    }

    pub fn open_session(&self) -> anyhow::Result<ChannelWrap> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(sess) => {
                let channel = sess.sess.channel_session()?;
                Ok(ChannelWrap::Ssh2(channel))
            }

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => {
                let channel = sess.sess.new_channel()?;
                channel.open_session()?;
                Ok(ChannelWrap::LibSsh(channel))
            }
        }
    }

    pub fn accept_agent_forward(&mut self) -> Option<ChannelWrap> {
        match self {
            // Unimplemented for now, an error message was printed earlier when the user tries to
            // enable agent forwarding so just return nothing here.
            #[cfg(feature = "ssh2")]
            Self::Ssh2(_sess) => None,

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh(sess) => sess.sess.accept_agent_forward().map(ChannelWrap::LibSsh),
        }
    }
}
