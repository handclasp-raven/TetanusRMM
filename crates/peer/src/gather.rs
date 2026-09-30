//! Candidate addresses for a direct path: where the other peer might reach
//! the UDP socket this peer will use.
//!
//! - **host**: the socket's own addresses, one per local interface (or the
//!   one it is bound to). Reachable on the same LAN.
//! - **server-reflexive**: the socket's public address, as the server's
//!   STUN responder saw it. Behind an ordinary NAT (one that keeps the same
//!   public port for the socket whoever it talks to, as home and office
//!   routers do), this is where the other peer can reach it once the NAT
//!   has been punched open (see `direct`).
//!
//! There are no relay (TURN) candidates: when neither kind works, the
//! session simply stays on the server's relay.
//!
//! Gathering uses the socket before QUIC takes it over, so the NAT mapping
//! STUN observed is the one the direct connection then uses.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use protocol::stun;
use ring::rand::{SecureRandom, SystemRandom};
use tracing::debug;

/// How long to wait for each STUN answer, and how many times to ask
/// (UDP may drop the request or the answer).
const STUN_WAIT: Duration = Duration::from_millis(300);
const STUN_TRIES: u32 = 4;

/// Candidates advertised (and accepted from the other peer) at most.
pub const MAX_CANDIDATES: usize = 8;

/// A bound socket and where it can be reached.
#[derive(Debug)]
pub struct Gathered {
    pub socket: UdpSocket,
    pub candidates: Vec<SocketAddr>,
    /// The public address STUN reported, if it answered.
    pub reflexive: Option<SocketAddr>,
}

/// Bind a UDP socket on `bind` (any port) and gather its candidates,
/// asking `stun` for the reflexive one.
pub async fn gather(bind: IpAddr, stun: Option<SocketAddr>) -> io::Result<Gathered> {
    let socket = UdpSocket::bind((bind, 0))?;
    socket.set_nonblocking(true)?;
    let port = socket.local_addr()?.port();
    let mut candidates = host_candidates(bind, port);
    let socket = tokio::net::UdpSocket::from_std(socket)?;
    let reflexive = match stun {
        Some(server) if server.is_ipv4() == bind.is_ipv4() => {
            let found = reflexive_address(&socket, server).await;
            debug!(%server, ?found, "STUN");
            found
        }
        _ => None,
    };
    // The public address first: it is the one that crosses NATs, and it
    // must survive the cap on machines with many interfaces.
    if let Some(addr) = reflexive {
        candidates.retain(|c| *c != addr);
        candidates.insert(0, addr);
    }
    candidates.truncate(MAX_CANDIDATES);
    Ok(Gathered {
        socket: socket.into_std()?,
        candidates,
        reflexive,
    })
}

/// The socket's addresses: the bound one, or every usable interface
/// address of the same family if bound to the unspecified address.
fn host_candidates(bind: IpAddr, port: u16) -> Vec<SocketAddr> {
    if !bind.is_unspecified() {
        return vec![SocketAddr::new(bind, port)];
    }
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddr> = interfaces
        .iter()
        .map(|i| i.ip())
        .filter(|ip| ip.is_ipv4() == bind.is_ipv4() && usable(ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    out.dedup();
    out
}

/// Loopback and link-local addresses never reach another machine.
fn usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// Ask `server` what address `socket` appears to come from.
async fn reflexive_address(
    socket: &tokio::net::UdpSocket,
    server: SocketAddr,
) -> Option<SocketAddr> {
    let mut txid = [0u8; 12];
    SystemRandom::new().fill(&mut txid).ok()?;
    let request = stun::binding_request(&txid);
    let mut buf = [0u8; 512];
    for _ in 0..STUN_TRIES {
        socket.send_to(&request, server).await.ok()?;
        let answer = tokio::time::timeout(STUN_WAIT, async {
            loop {
                let (n, from) = socket.recv_from(&mut buf).await.ok()?;
                if from != server {
                    continue;
                }
                if let Some(mapped) = stun::parse_binding_response(&buf[..n], &txid) {
                    return Some(mapped);
                }
            }
        })
        .await;
        if let Ok(found) = answer {
            return found;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STUN responder like the server's.
    async fn responder() -> SocketAddr {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let (n, from) = socket.recv_from(&mut buf).await.unwrap();
                if let Some(txid) = stun::parse_binding_request(&buf[..n]) {
                    let _ = socket
                        .send_to(&stun::binding_response(&txid, from), from)
                        .await;
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn gathers_the_bound_address_and_the_reflexive_one() {
        let stun = responder().await;
        let g = gather("127.0.0.1".parse().unwrap(), Some(stun))
            .await
            .unwrap();
        let local = g.socket.local_addr().unwrap();
        // On loopback there is no NAT: the reflexive address is the host one.
        assert_eq!(g.reflexive, Some(local));
        assert_eq!(g.candidates, [local]);
    }

    #[tokio::test]
    async fn no_stun_answer_leaves_host_candidates_only() {
        // Bound but never answers.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let g = gather(
            "127.0.0.1".parse().unwrap(),
            Some(silent.local_addr().unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(g.reflexive, None);
        assert_eq!(g.candidates, [g.socket.local_addr().unwrap()]);
    }

    #[tokio::test]
    async fn the_reflexive_address_comes_first() {
        // A STUN server that reports a (made-up) public address.
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun = socket.local_addr().unwrap();
        let public: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let txid = stun::parse_binding_request(&buf[..n]).unwrap();
            socket
                .send_to(&stun::binding_response(&txid, public), from)
                .await
                .unwrap();
        });
        let g = gather("127.0.0.1".parse().unwrap(), Some(stun))
            .await
            .unwrap();
        assert_eq!(g.candidates, [public, g.socket.local_addr().unwrap()]);
    }

    #[test]
    fn unspecified_binds_list_real_interfaces_only() {
        for c in host_candidates("0.0.0.0".parse().unwrap(), 5000) {
            assert!(c.is_ipv4() && usable(&c.ip()) && c.port() == 5000, "{c}");
        }
    }
}
