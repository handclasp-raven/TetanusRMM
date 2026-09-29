//! End-to-end tests: real server and agent, in-process, over loopback QUIC.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use agent::{AgentConfig, AgentEvent, AgentSession};
use common::devcerts::{self, DevCerts};
use protocol::{close_code, read_frame, write_frame, Message};
use server::{Server, ServerConfig, ServerEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::time::timeout;

const HEARTBEAT: Duration = Duration::from_millis(50);
const DEADLINE: Duration = Duration::from_secs(10);
const LOOPBACK: &str = "127.0.0.1:0";

fn start_server(certs: &DevCerts) -> (Arc<Server>, SocketAddr, UnboundedReceiver<ServerEvent>) {
    let (tx, rx) = unbounded_channel();
    let server = Server::bind(ServerConfig {
        listen: LOOPBACK.parse().unwrap(),
        identity: certs.server_identity().unwrap(),
        client_ca: certs.ca().unwrap(),
    })
    .unwrap()
    .with_events(tx);
    let addr = server.local_addr().unwrap();
    let server = Arc::new(server);
    let runner = server.clone();
    tokio::spawn(async move { runner.run().await });
    (server, addr, rx)
}

fn agent_config(server_addr: SocketAddr, trust: &DevCerts, client: &DevCerts) -> AgentConfig {
    AgentConfig {
        server_addr,
        server_name: "localhost".into(),
        agent_id: "test-agent".into(),
        server_ca: trust.ca().unwrap(),
        identity: client.agent_identity().unwrap(),
        heartbeat_interval: HEARTBEAT,
        bind_addr: Some(LOOPBACK.parse().unwrap()),
        telemetry: None,
    }
}

/// Start an agent session in the background, returning it and its ack stream.
async fn start_agent(config: &AgentConfig) -> (Arc<AgentSession>, UnboundedReceiver<AgentEvent>) {
    let session = Arc::new(agent::connect(config).await.unwrap());
    let (tx, rx) = unbounded_channel();
    let runner = session.clone();
    tokio::spawn(async move { runner.run(Some(tx)).await });
    (session, rx)
}

/// Receive from `rx` into `seen` until `done(seen)` holds. Panics on timeout.
async fn collect_until<T: std::fmt::Debug>(
    rx: &mut UnboundedReceiver<T>,
    seen: &mut Vec<T>,
    done: impl Fn(&[T]) -> bool,
) {
    let result = timeout(DEADLINE, async {
        while !done(seen) {
            seen.push(rx.recv().await.expect("event channel closed"));
        }
    })
    .await;
    assert!(result.is_ok(), "timed out; events so far: {seen:#?}");
}

fn heartbeats(events: &[ServerEvent]) -> Vec<(usize, u64, SocketAddr)> {
    events
        .iter()
        .filter_map(|e| match e {
            ServerEvent::Heartbeat {
                conn_id,
                seq,
                remote,
                ..
            } => Some((*conn_id, *seq, *remote)),
            _ => None,
        })
        .collect()
}

fn hellos(events: &[ServerEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, ServerEvent::Hello { .. }))
        .count()
}

#[tokio::test]
async fn agent_says_hello_and_heartbeats_are_acked() {
    let certs = devcerts::generate("test-agent").unwrap();
    let (_server, addr, mut server_rx) = start_server(&certs);
    let (_agent, mut agent_rx) = start_agent(&agent_config(addr, &certs, &certs)).await;

    let mut server_seen = Vec::new();
    collect_until(&mut server_rx, &mut server_seen, |e| {
        heartbeats(e).len() >= 2
    })
    .await;
    let mut acks = Vec::new();
    collect_until(&mut agent_rx, &mut acks, |a| a.len() >= 2).await;

    match &server_seen[0] {
        ServerEvent::Hello {
            agent_id, version, ..
        } => {
            assert_eq!(agent_id, "test-agent");
            assert_eq!(*version, protocol::PROTOCOL_VERSION);
        }
        other => panic!("first event should be Hello, got {other:?}"),
    }
    let seqs: Vec<u64> = heartbeats(&server_seen).iter().map(|h| h.1).collect();
    assert_eq!(&seqs[..2], &[0, 1]);
    assert_eq!(
        &acks[..2],
        &[AgentEvent::Ack { seq: 0 }, AgentEvent::Ack { seq: 1 }]
    );
}

#[tokio::test]
async fn connection_survives_local_address_change() {
    let certs = devcerts::generate("test-agent").unwrap();
    let (_server, addr, mut server_rx) = start_server(&certs);
    let (agent, mut agent_rx) = start_agent(&agent_config(addr, &certs, &certs)).await;

    let mut server_seen = Vec::new();
    collect_until(&mut server_rx, &mut server_seen, |e| {
        heartbeats(e).len() >= 2
    })
    .await;
    let (conn_id, _, old_remote) = heartbeats(&server_seen)[0];
    let old_local = agent.local_addr().unwrap();
    assert_eq!(old_local, old_remote);

    // Simulate an interface change: move the agent onto a new UDP socket.
    agent.rebind(UdpSocket::bind(LOOPBACK).unwrap()).unwrap();
    let new_local = agent.local_addr().unwrap();
    assert_ne!(new_local, old_local);

    // The server keeps receiving heartbeats on the same connection, now from
    // the new address, and the agent keeps getting acks.
    collect_until(&mut server_rx, &mut server_seen, |e| {
        heartbeats(e)
            .iter()
            .filter(|(_, _, remote)| *remote == new_local)
            .count()
            >= 2
    })
    .await;
    for (id, _, remote) in heartbeats(&server_seen) {
        assert_eq!(id, conn_id, "heartbeat arrived on a new connection");
        assert!(remote == old_local || remote == new_local);
    }
    assert_eq!(
        hellos(&server_seen),
        1,
        "agent reconnected instead of migrating"
    );

    let migrated_seq = heartbeats(&server_seen)
        .iter()
        .find(|h| h.2 == new_local)
        .unwrap()
        .1;
    let mut acks = Vec::new();
    collect_until(&mut agent_rx, &mut acks, |a| {
        a.contains(&AgentEvent::Ack {
            seq: migrated_seq + 1,
        })
    })
    .await;
}

#[tokio::test]
async fn server_rejects_client_cert_from_unknown_ca() {
    let certs = devcerts::generate("test-agent").unwrap();
    let rogue = devcerts::generate("rogue-agent").unwrap();
    let (_server, addr, mut server_rx) = start_server(&certs);

    // Trusts the real server, but presents a cert signed by another CA.
    let config = agent_config(addr, &certs, &rogue);
    let result = timeout(DEADLINE, async {
        agent::connect(&config).await?.run(None).await
    })
    .await
    .expect("rejection should not hang");
    assert!(result.is_err());
    assert!(server_rx.try_recv().is_err(), "server must not see a Hello");
}

#[tokio::test]
async fn server_rejects_connection_without_client_cert() {
    let certs = devcerts::generate("test-agent").unwrap();
    let (_server, addr, mut server_rx) = start_server(&certs);

    let mut endpoint = quinn::Endpoint::client(LOOPBACK.parse().unwrap()).unwrap();
    endpoint.set_default_client_config(
        common::quic::client_config(&certs.ca().unwrap(), None).unwrap(),
    );
    let result = timeout(DEADLINE, async {
        let conn = endpoint.connect(addr, "localhost")?.await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_frame(
            &mut send,
            &Message::Hello {
                agent_id: "anon".into(),
                version: protocol::PROTOCOL_VERSION,
            },
        )
        .await?;
        Ok::<_, anyhow::Error>(read_frame::<_, Message>(&mut recv).await?)
    })
    .await
    .expect("rejection should not hang");
    assert!(result.is_err(), "expected failure, got {result:?}");
    assert!(server_rx.try_recv().is_err(), "server must not see a Hello");
}

#[tokio::test]
async fn agent_rejects_server_cert_from_unknown_ca() {
    let certs = devcerts::generate("test-agent").unwrap();
    let other = devcerts::generate("test-agent").unwrap();
    let (_server, addr, _server_rx) = start_server(&certs);

    // Valid client cert for this server, but trusts a different CA.
    let mut config = agent_config(addr, &certs, &certs);
    config.server_ca = other.ca().unwrap();
    let result = timeout(DEADLINE, agent::connect(&config))
        .await
        .expect("rejection should not hang");
    assert!(result.is_err());
}

#[tokio::test]
async fn server_closes_connection_that_skips_hello() {
    let certs = devcerts::generate("test-agent").unwrap();
    let (_server, addr, mut server_rx) = start_server(&certs);

    let mut endpoint = quinn::Endpoint::client(LOOPBACK.parse().unwrap()).unwrap();
    endpoint.set_default_client_config(
        common::quic::client_config(&certs.ca().unwrap(), Some(&certs.agent_identity().unwrap()))
            .unwrap(),
    );
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    write_frame(
        &mut send,
        &Message::Heartbeat {
            ts: 0,
            seq: 0,
            telemetry: None,
        },
    )
    .await
    .unwrap();

    let reason = timeout(DEADLINE, conn.closed())
        .await
        .expect("server should close");
    match reason {
        quinn::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(close.error_code, close_code::PROTOCOL_ERROR.into());
        }
        other => panic!("expected application close, got {other:?}"),
    }
    assert!(
        server_rx.try_recv().is_err(),
        "no Hello or Heartbeat should be reported"
    );
}
