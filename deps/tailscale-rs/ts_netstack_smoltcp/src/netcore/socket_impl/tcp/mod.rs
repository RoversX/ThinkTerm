use alloc::vec;

use smoltcp::socket::tcp;

use crate::netcore;

mod listener;
mod stream;

pub use listener::{ListenerHandle, TcpListenerState};

impl netcore::Netstack {
    fn tcp_buffer(&self) -> tcp::SocketBuffer<'static> {
        tcp::SocketBuffer::new(vec![0; self.config.tcp_buffer_size])
    }
}

// ThinkTerm (lost peers): everything from here down is ours.

/// How long a connection may go without a packet from its peer -- an answer to what it sent, or
/// to a keep-alive -- before it is aborted, and how long it may sit idle before a keep-alive
/// asks. Without them a peer that goes away without a word, a laptop put to sleep, leaves the
/// connection retrying forever, and a connect nobody waits for any more lingers too.
const PEER_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(600);
const KEEP_ALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(120);

/// Has `socket` notice a peer that went away, as the system's own TCP would.
fn notice_lost_peers(socket: &mut tcp::Socket) {
    socket.set_timeout(Some(PEER_TIMEOUT));
    socket.set_keep_alive(Some(KEEP_ALIVE));
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::net::SocketAddr;

    use smoltcp::{socket::Socket, time::Duration, time::Instant};

    use super::{KEEP_ALIVE, PEER_TIMEOUT};
    use crate::netcore::{
        Config, Netstack,
        command::tcp::{listen::Command as TcpListenCommand, stream::Command as TcpStreamCommand},
    };

    /// The timeout and keep-alive of every TCP socket the netstack holds.
    fn liveness(netstack: &Netstack) -> Vec<(Option<Duration>, Option<Duration>)> {
        netstack
            .socket_set
            .iter()
            .filter_map(|(_, socket)| match socket {
                Socket::Tcp(socket) => Some((socket.timeout(), socket.keep_alive())),
                _ => None,
            })
            .collect()
    }

    fn addr(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn a_connection_notices_a_peer_that_went_away() {
        let mut netstack = Netstack::new(Config::default(), Instant::ZERO);
        netstack.process_tcp_stream(
            TcpStreamCommand::Connect {
                local_endpoint: addr("192.0.2.1:40000"),
                remote_endpoint: addr("198.51.100.7:22"),
            },
            None,
        );
        assert_eq!(liveness(&netstack), [(Some(PEER_TIMEOUT), Some(KEEP_ALIVE))]);
    }

    #[test]
    fn a_listener_notices_a_peer_that_went_away() {
        let mut netstack = Netstack::new(Config::default(), Instant::ZERO);
        netstack.process_tcp_listen(
            TcpListenCommand::Listen {
                local_endpoint: addr("192.0.2.1:22"),
            },
            None,
        );
        assert_eq!(liveness(&netstack), [(Some(PEER_TIMEOUT), Some(KEEP_ALIVE))]);
    }
}
