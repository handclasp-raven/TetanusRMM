//! Agent and viewer behind two real NATs (Linux only).
//!
//! The test re-runs itself (`nat_lab_driver`) inside a fresh unprivileged
//! user and network namespace, the "internet", where it builds this with
//! veth pairs and nftables and runs the real server:
//!
//! ```text
//!   agent 192.168.1.2 ── NAT A ── 10.0.1.0/24 ─┐
//!                       (10.0.1.2)             internet: server 10.0.1.1
//!   viewer 192.168.1.2 ─ NAT B ── 10.0.2.0/24 ─┘         (QUIC, STUN)
//!                       (10.0.2.2)
//! ```
//!
//! Both LANs use the same private subnet, as real ones do, so a host
//! candidate never reaches the other side. The agent and the viewer run on
//! threads that have entered their LAN's namespace, so every socket they
//! open (to the server, for STUN, for the direct path) lives behind their
//! NAT. Postgres stays outside; the server reaches it over a Unix socket.
//!
//! - **Ordinary NAT** (`masquerade`: a socket keeps its public port whoever
//!   it talks to): the session must go direct, through the NATs' public
//!   addresses, confirmed by the server's metrics.
//! - **Symmetric NAT** (`masquerade random,fully-random`: a new public
//!   port per destination): punching cannot work, so the session must stay
//!   on the relay, and keep working.
//!
//! Needs unprivileged user namespaces, `ip`, `nft` and `nsenter`; where
//! they are missing the tests say so and pass without running.

#![cfg(target_os = "linux")]

mod support;

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use agent::media::source::media_channel;
use protocol::e2e::Path;
use support::source::{run_source, SourceLog};
use viewer::client::{self, ViewerEvent, ViewerOptions};
use viewer::decode::VideoDecoder;

const LAB_ENV: &str = "RMM_NAT_LAB";

/// Builds the topology. `$1`: extra masquerade flags. Prints the PIDs
/// holding the agent's and the viewer's LAN namespaces, then every PID to
/// clean up.
const TOPOLOGY: &str = r#"
set -e
mkns() { unshare -n sleep 600 >/dev/null 2>&1 & echo $!; }
NATA=$(mkns); HOSTA=$(mkns); NATB=$(mkns); HOSTB=$(mkns)
sleep 0.2
inns() { local p=$1; shift; nsenter -t "$p" -n "$@"; }
ip link set lo up
ip link add wa type veth peer name wan netns "$NATA"
ip link add wb type veth peer name wan netns "$NATB"
ip addr add 10.0.1.1/24 dev wa; ip link set wa up
ip addr add 10.0.2.1/24 dev wb; ip link set wb up
echo 1 > /proc/sys/net/ipv4/ip_forward
for side in A B; do
  if [ $side = A ]; then NAT=$NATA; HOST=$HOSTA; SUB=1; else NAT=$NATB; HOST=$HOSTB; SUB=2; fi
  inns $NAT ip link set lo up
  inns $NAT ip addr add 10.0.$SUB.2/24 dev wan; inns $NAT ip link set wan up
  inns $NAT ip route add default via 10.0.$SUB.1
  inns $NAT ip link add lan type veth peer name eth0 netns "$HOST"
  inns $NAT ip addr add 192.168.1.1/24 dev lan; inns $NAT ip link set lan up
  inns $NAT sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'
  inns $NAT nft add table ip nat
  inns $NAT nft add chain ip nat post '{ type nat hook postrouting priority 100; }'
  inns $NAT nft add rule ip nat post oifname wan masquerade $1
  inns $HOST ip link set lo up
  inns $HOST ip addr add 192.168.1.2/24 dev eth0; inns $HOST ip link set eth0 up
  inns $HOST ip route add default via 192.168.1.1
done
echo "$HOSTA $HOSTB"
echo "$NATA $HOSTA $NATB $HOSTB"
"#;

/// The server, as seen from both LANs.
const SERVER_IP: &str = "10.0.1.1";
/// NAT A's public address.
const NAT_A: &str = "10.0.1.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nat {
    Ordinary,
    Symmetric,
}

impl Nat {
    fn name(self) -> &'static str {
        match self {
            Nat::Ordinary => "ordinary",
            Nat::Symmetric => "symmetric",
        }
    }

    fn masquerade_flags(self) -> &'static str {
        match self {
            Nat::Ordinary => "",
            Nat::Symmetric => "random,fully-random",
        }
    }
}

/// Whether this machine can build the lab.
fn lab_available() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("not Linux".into());
    }
    for tool in ["unshare", "nsenter", "ip", "nft"] {
        let found = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            return Err(format!("{tool} is not installed"));
        }
    }
    let userns = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !userns {
        return Err("unprivileged user namespaces are not available".into());
    }
    Ok(())
}

/// Run the lab for `nat` in a new namespace and check it passed.
async fn run_lab(nat: Nat) {
    if let Err(why) = lab_available() {
        eprintln!("SKIPPED: the NAT lab cannot run here: {why}");
        return;
    }
    let db = support::start_db().await;
    // The namespace cannot reach the container over TCP, but it shares the
    // filesystem: forward a Unix socket to it.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join(".s.PGSQL.5432");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let pg_port = db.port;
    tokio::spawn(async move {
        loop {
            let Ok((mut unix, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                if let Ok(mut tcp) = tokio::net::TcpStream::connect(("127.0.0.1", pg_port)).await {
                    let _ = tokio::io::copy_bidirectional(&mut unix, &mut tcp).await;
                }
            });
        }
    });

    let exe = std::env::current_exe().unwrap();
    let spec = format!("{}:{}", nat.name(), dir.path().display());
    // The lab bounds its own waits, so this ends.
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--net", "--"])
            .arg(exe)
            .args(["--exact", "nat_lab_driver", "--ignored", "--nocapture"])
            .env(LAB_ENV, spec)
            .output()
    })
    .await
    .unwrap()
    .unwrap();
    let log = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    let passed = log.contains("NAT LAB PASSED");
    if !passed || !output.status.success() {
        panic!("NAT lab ({}) failed:\n{log}", nat.name());
    }
    // Show what it saw (visible with --nocapture).
    for line in log.lines().filter(|l| l.starts_with("lab:")) {
        println!("{line}");
    }
}

#[tokio::test]
async fn peers_behind_ordinary_nats_connect_directly() {
    run_lab(Nat::Ordinary).await;
}

#[tokio::test]
async fn peers_behind_symmetric_nats_stay_on_the_relay_and_work() {
    run_lab(Nat::Symmetric).await;
}

// --- Inside the namespace ---------------------------------------------------

/// Run by `run_lab` inside the new namespace; does nothing otherwise.
#[test]
#[ignore = "run by the NAT lab tests inside their network namespace"]
fn nat_lab_driver() {
    let Ok(spec) = std::env::var(LAB_ENV) else {
        return;
    };
    // RUST_LOG passes through; the outer test prints it all on failure.
    common::logging::init();
    let (mode, pg_dir) = spec.split_once(':').unwrap();
    let nat = match mode {
        "ordinary" => Nat::Ordinary,
        "symmetric" => Nat::Symmetric,
        other => panic!("unknown mode {other}"),
    };
    let topology = std::process::Command::new("bash")
        .args(["-c", TOPOLOGY, "topology", nat.masquerade_flags()])
        .output()
        .unwrap();
    assert!(
        topology.status.success(),
        "building the topology failed: {}",
        String::from_utf8_lossy(&topology.stderr)
    );
    let mut lines = topology.stdout.lines().map(Result::unwrap);
    let hosts: Vec<u32> = parse_pids(&lines.next().unwrap());
    let all: Vec<u32> = parse_pids(&lines.next().unwrap());
    let (agent_ns, viewer_ns) = (hosts[0], hosts[1]);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(lab(nat, pg_dir, agent_ns, viewer_ns))
    }));
    for pid in all {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    println!("NAT LAB PASSED");
    std::io::stdout().flush().unwrap();
    // Agent and viewer threads are still running; do not wait for them.
    std::process::exit(0);
}

fn parse_pids(line: &str) -> Vec<u32> {
    line.split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// Run `f` on a new thread inside the network namespace of process `pid`,
/// with its own runtime: every socket it opens belongs to that namespace.
fn in_namespace<T: Send + 'static>(
    pid: u32,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::spawn(move || {
        let ns = std::fs::File::open(format!("/proc/{pid}/ns/net")).unwrap();
        // SAFETY: a plain syscall on a valid descriptor; it only changes
        // this thread's network namespace.
        let rc = unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) };
        assert_eq!(rc, 0, "setns: {}", std::io::Error::last_os_error());
        f()
    })
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// What the viewer saw.
#[derive(Debug)]
struct Seen {
    paths: Vec<Path>,
    direct_remote: Option<SocketAddr>,
    /// Pictures decoded after the path settled.
    pictures_after: usize,
    /// Whether the session was still relayed at the end.
    path: Path,
}

async fn lab(nat: Nat, pg_dir: &str, agent_ns: u32, viewer_ns: u32) {
    let options = sqlx::postgres::PgConnectOptions::new()
        .socket(pg_dir)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .unwrap();
    server::db::MIGRATOR.run(&pool).await.unwrap();

    // The server, on the "internet".
    let certs = common::devcerts::generate("unused").unwrap();
    let stun = server::stun::bind("0.0.0.0:0".parse().unwrap())
        .await
        .unwrap();
    let stun_port = stun.local_addr().unwrap().port();
    tokio::spawn(server::stun::serve(stun));
    let quic = server::Server::bind(server::ServerConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        ws_listen: None,
        identity: certs.server_identity().unwrap(),
        client_ca: certs.ca().unwrap(),
    })
    .unwrap()
    .with_registry(server::Registry {
        pool: pool.clone(),
        ca: server::enroll::AgentCa::from_pem(&certs.ca_cert, &certs.ca_key).unwrap(),
        api_url: "https://localhost:8443".into(),
    })
    .with_direct_paths(true, Some(stun_port));
    let server_addr: SocketAddr = SocketAddr::new(
        SERVER_IP.parse().unwrap(),
        quic.local_addr().unwrap().port(),
    );
    let hub = quic.hub();
    let quic = Arc::new(quic);
    tokio::spawn({
        let quic = quic.clone();
        async move { quic.run().await }
    });
    println!(
        "lab: {} NATs; server at {server_addr}, STUN port {stun_port}",
        nat.name()
    );

    // The agent, behind NAT A: enrolls, then serves a synthetic screen.
    let token = server::enroll::create_token(&pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let ca_pem = certs.ca_cert.clone();
    let (enrolled_tx, enrolled) = tokio::sync::oneshot::channel();
    let _agent = in_namespace(agent_ns, move || {
        current_thread_runtime().block_on(async move {
            let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
                server_addr,
                transport: Default::default(),
                server_name: "localhost".into(),
                server_ca_pem: ca_pem,
                token,
                bind_addr: None,
            })
            .await
            .unwrap();
            let (media, source) = media_channel();
            run_source(source, Arc::new(SourceLog::default()));
            let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
            config.media = Some(Arc::new(media));
            let session = agent::connect(&config).await.unwrap();
            let _ = enrolled_tx.send(credential.agent_id.clone());
            let _ = session.run(None).await;
        })
    });
    let agent_id = tokio::time::timeout(Duration::from_secs(30), enrolled)
        .await
        .expect("agent enrolled through NAT A")
        .unwrap();
    // Its Hello has reached the relay once it is online.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !hub.is_online(&agent_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("agent online");

    // The viewer, behind NAT B.
    let admin = support::create_user(&pool, "root", server::users::Role::Admin).await;
    let token = server::viewers::create(&pool, &admin.user, &agent_id)
        .await
        .unwrap()
        .token;
    let ca_pem = certs.ca_cert.clone();
    let seen = in_namespace(viewer_ns, move || {
        current_thread_runtime().block_on(watch(ViewerOptions {
            server: server_addr,
            transport: Default::default(),
            server_name: "localhost".into(),
            ca_pem,
            token,
            bind: None,
            direct: Default::default(),
        }))
    });
    let seen = tokio::task::spawn_blocking(move || seen.join().unwrap())
        .await
        .unwrap();
    println!("lab: viewer saw {seen:?}");

    let metric = |name: &str| -> f64 {
        server::metrics::get()
            .render()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .map_or(0.0, |v| v.trim().parse().unwrap())
    };
    let direct = metric("rmm_viewers_by_path{path=\"direct\"}");
    let relayed = metric("rmm_viewers_by_path{path=\"relayed\"}");
    let failed = metric("rmm_session_path_changes_total{path=\"direct_failed\"}");
    let stun_requests = metric("rmm_stun_requests_total");
    println!("lab: metrics: viewers direct={direct} relayed={relayed}; direct_failed={failed}; stun_requests={stun_requests}");
    assert!(stun_requests >= 2.0, "both peers asked STUN");
    assert!(seen.pictures_after >= 30, "{seen:?}");

    match nat {
        Nat::Ordinary => {
            assert_eq!(seen.paths, [Path::Direct]);
            assert_eq!(seen.path, Path::Direct);
            // Through NAT A's public address: a hole punched, not a LAN.
            let remote = seen.direct_remote.expect("a direct path");
            assert_eq!(remote.ip().to_string(), NAT_A, "direct to {remote}");
            assert_eq!((direct, relayed), (1.0, 0.0), "metrics confirm the path");
            let frames = metric("rmm_relay_frames_received_total");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                metric("rmm_relay_frames_received_total"),
                frames,
                "video no longer goes through the server"
            );
        }
        Nat::Symmetric => {
            assert_eq!(seen.paths, [Path::DirectFailed], "never went direct");
            assert_eq!(seen.path, Path::Relayed);
            assert_eq!((direct, relayed, failed), (0.0, 1.0, 1.0));
        }
    }
}

/// Connect, and watch until the path settles (direct, or a failed
/// attempt), then decode a while longer.
async fn watch(options: ViewerOptions) -> Seen {
    let (_welcome, handle, mut events) = client::connect(&options).await.unwrap();
    let mut decoder = VideoDecoder::new().unwrap();
    let mut paths = Vec::new();
    let mut pictures_after = 0;
    let give_up = tokio::time::Instant::now() + Duration::from_secs(40);
    let mut settled: Option<tokio::time::Instant> = None;
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            settled.is_some() || now < give_up,
            "the path never settled: {paths:?}"
        );
        // Settled: direct, or the attempt failed.
        if settled.is_none() && !paths.is_empty() {
            settled = Some(now);
        }
        if settled.is_some() && pictures_after >= 30 {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(20), events.recv()).await {
            Ok(Some(ViewerEvent::Frame(frame))) => match decoder.decode(&frame) {
                Ok(Some(_)) if settled.is_some() => pictures_after += 1,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("lab: decode error at {}: {e}", frame.seq);
                    handle.request_keyframe();
                }
            },
            Ok(Some(ViewerEvent::Path(path))) => paths.push(path),
            Ok(Some(ViewerEvent::Closed(reason))) => panic!("viewer closed: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => panic!("no video for 20 s (paths {paths:?})"),
        }
    }
    Seen {
        paths,
        direct_remote: handle.direct_remote(),
        pictures_after,
        path: handle.path(),
    }
}
