//! A STUN responder (Binding requests only, see `protocol::stun`), so
//! agents and viewers can learn the public address of the UDP socket they
//! will use for a direct path. It tells each requester the address its
//! request came from, and nothing else: no state, no authentication. An
//! answer is the size of a request, so it cannot be used to amplify
//! traffic.
//!
//! The port is announced to agents and viewers in `PeerConfig`; it must be
//! reachable on the same host address as the QUIC listener, and the server
//! must see clients' real source addresses (in Docker, publish it as UDP
//! on the host, which preserves them).

use std::io::IoSliceMut;
use std::net::{IpAddr, SocketAddr};

use protocol::stun;
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tracing::{info, warn};

/// Bind the responder.
pub async fn bind(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    UdpSocket::bind(addr).await
}

/// Answer Binding requests on `socket` forever.
///
/// Each answer leaves from the address its request was sent to. On a
/// server with several addresses, bound to the unspecified one (the
/// default), the OS would otherwise pick the source by route, and a NAT
/// would drop an answer from an address its client never sent to. So the
/// socket goes through `quinn-udp`, which reads each datagram's
/// destination and sets the source of the reply, as quinn does for QUIC.
pub async fn serve(socket: UdpSocket) {
    info!(addr = ?socket.local_addr().ok(), "STUN responder listening");
    let state = match quinn_udp::UdpSocketState::new((&socket).into()) {
        Ok(state) => state,
        Err(e) => return warn!("STUN responder disabled: {e}"),
    };
    let mut buf = [0u8; 1500];
    loop {
        let mut meta = [quinn_udp::RecvMeta::default()];
        let received = socket
            .async_io(Interest::READABLE, || {
                state.recv(
                    (&socket).into(),
                    &mut [IoSliceMut::new(&mut buf)],
                    &mut meta,
                )
            })
            .await;
        match received {
            Ok(n) if n > 0 => {}
            Ok(_) => continue,
            // E.g. ICMP port unreachable from an earlier answer (Windows
            // reports those on the socket): not fatal.
            Err(e) => {
                warn!("STUN receive: {e}");
                continue;
            }
        }
        let meta = meta[0];
        let len = meta.len.min(meta.stride.max(1)).min(buf.len());
        let Some(txid) = stun::parse_binding_request(&buf[..len]) else {
            continue;
        };
        crate::metrics::get().stun_requests.inc();
        let response = stun::binding_response(&txid, unmapped(meta.addr));
        let transmit = quinn_udp::Transmit {
            destination: meta.addr,
            ecn: None,
            contents: &response,
            segment_size: None,
            src_ip: meta.dst_ip,
        };
        let _ = socket
            .async_io(Interest::WRITABLE, || {
                state.try_send((&socket).into(), &transmit)
            })
            .await;
    }
}

/// An IPv4 client seen through a dual-stack socket is an IPv4 client.
fn unmapped(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
            None => addr,
        },
        IpAddr::V4(_) => addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_with_the_requesters_address_and_ignores_the_rest() {
        let socket = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(serve(socket));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"not stun", addr).await.unwrap();
        let txid = *b"abcdefghijkl";
        client
            .send_to(&stun::binding_request(&txid), addr)
            .await
            .unwrap();
        let mut buf = [0u8; 512];
        let (n, from) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.recv_from(&mut buf),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(from, addr);
        assert_eq!(
            stun::parse_binding_response(&buf[..n], &txid),
            Some(client.local_addr().unwrap())
        );
    }
}
