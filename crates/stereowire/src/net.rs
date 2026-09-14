//! UDP transport.
//!
//! The link is expected to run inside Tailscale, which already provides
//! encryption, authentication, and NAT traversal, so this layer stays a thin
//! datagram pipe.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};

pub struct Link {
    socket: UdpSocket,
}

impl Link {
    /// Outbound side: an ephemeral local port aimed at `peer`.
    ///
    /// Returns the link and the address it resolved to, so the caller can check
    /// whether that address is inside the tunnel this tool depends on.
    pub fn connect(peer: &str, port: u16) -> Result<(Self, SocketAddr)> {
        let addr = (peer, port)
            .to_socket_addrs()
            .with_context(|| format!("could not resolve {peer}"))?
            .next()
            .with_context(|| format!("no address for {peer}"))?;
        let socket = UdpSocket::bind("0.0.0.0:0").context("could not bind local socket")?;
        socket
            .connect(addr)
            .with_context(|| format!("could not reach {addr}"))?;
        // Control replies are polled, so never block the send path on them.
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok((Link { socket }, addr))
    }

    /// Inbound side: listens on `port` for whichever peer connects.
    pub fn listen(port: u16) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .with_context(|| format!("could not bind port {port}"))?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(Link { socket })
    }

    pub fn send(&self, datagram: &[u8]) {
        // A dropped datagram is the transport working as designed; the jitter
        // buffer and keyframe requests handle the consequences.
        let _ = self.socket.send(datagram);
    }

    pub fn send_to(&self, datagram: &[u8], peer: SocketAddr) {
        let _ = self.socket.send_to(datagram, peer);
    }

    /// Returns `None` on timeout rather than treating it as an error.
    pub fn recv(&self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        self.socket.recv_from(buf).ok()
    }

    /// The address actually bound, which matters when listening on port 0.
    #[cfg(test)]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagrams_survive_a_real_socket_round_trip() {
        // Port 0 lets the OS pick, so tests never collide on a fixed port.
        let receiver = Link::listen(0).expect("binds");
        let port = receiver.local_addr().expect("bound").port();
        let (sender, _) = Link::connect("127.0.0.1", port).expect("connects");

        sender.send(b"hello");
        let mut buf = [0u8; 64];
        let (len, peer) = receiver.recv(&mut buf).expect("receives");
        assert_eq!(&buf[..len], b"hello");

        // The receiver must be able to reply to whoever it heard from, which is
        // how keyframe requests and latency probes get home.
        receiver.send_to(b"pong", peer);
        let mut back = [0u8; 64];
        let (len, _) = sender.recv(&mut back).expect("receives the reply");
        assert_eq!(&back[..len], b"pong");
    }

    #[test]
    fn recv_returns_none_on_timeout_rather_than_erroring() {
        let idle = Link::listen(0).expect("binds");
        let mut buf = [0u8; 64];
        assert!(
            idle.recv(&mut buf).is_none(),
            "a quiet socket must simply time out"
        );
    }

    #[test]
    fn an_unresolvable_peer_is_reported() {
        assert!(Link::connect("no-such-host.invalid", 9000).is_err());
    }
}
