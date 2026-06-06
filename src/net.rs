use crate::state::{unix_ms, InboxMsg, PeerInfo, Shared};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

/// Fixed multicast group for claude-lan discovery (administratively scoped, LAN-only).
pub const MCAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 92, 108, 99);

const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);
const PEER_STALE_AFTER: Duration = Duration::from_secs(30);

pub fn mcast_port() -> u16 {
    std::env::var("CLAUDE_LAN_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48618)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Packet {
    /// Active probe: "who's out there?" — peers reply with Here.
    Probe { id: String, name: String, port: u16 },
    /// Presence announcement / probe reply. Sent to the multicast group (not
    /// unicast) so that multiple instances sharing one host via SO_REUSEPORT
    /// all see every reply.
    Here { id: String, name: String, port: u16 },
    /// Clean shutdown.
    Bye { id: String },
}

/// Bind the shared discovery socket: reuseaddr+reuseport so several instances
/// can coexist on one machine, multicast loop on so they hear each other.
pub fn bind_discovery(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    let sock: std::net::UdpSocket = s.into();
    sock.join_multicast_v4(&MCAST_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    sock.set_multicast_loop_v4(true)?;
    sock.set_multicast_ttl_v4(1)?;
    sock.set_broadcast(true)?;
    UdpSocket::from_std(sock)
}

async fn send_packet(sock: &UdpSocket, port: u16, p: &Packet) {
    let buf = serde_json::to_vec(p).unwrap();
    // Multicast is the primary channel; broadcast is a fallback for networks
    // that filter multicast. Duplicate receipt is harmless (upsert is idempotent).
    let _ = sock.send_to(&buf, (MCAST_GROUP, port)).await;
    let _ = sock.send_to(&buf, (Ipv4Addr::BROADCAST, port)).await;
}

fn here_packet(shared: &Shared) -> Packet {
    Packet::Here {
        id: shared.id.clone(),
        name: shared.name.clone(),
        port: shared.tcp_port,
    }
}

fn upsert_peer(shared: &Shared, id: String, name: String, addr: IpAddr, tcp_port: u16, rtt: Option<u64>) {
    let mut peers = shared.peers.lock().unwrap();
    let entry = peers.entry(id.clone()).or_insert_with(|| PeerInfo {
        id,
        name: String::new(),
        addr,
        tcp_port,
        last_seen: Instant::now(),
        rtt_micros: None,
    });
    entry.name = name;
    entry.addr = addr;
    entry.tcp_port = tcp_port;
    entry.last_seen = Instant::now();
    if rtt.is_some() {
        entry.rtt_micros = rtt;
    }
}

/// React to discovery traffic: answer probes, learn peers, drop the departed.
pub async fn discovery_listener(shared: Arc<Shared>, sock: Arc<UdpSocket>, port: u16) {
    let mut buf = [0u8; 4096];
    loop {
        let Ok((n, src)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        let Ok(pkt) = serde_json::from_slice::<Packet>(&buf[..n]) else {
            continue;
        };
        match pkt {
            Packet::Probe { id, name, port: tcp } => {
                if id == shared.id {
                    continue;
                }
                upsert_peer(&shared, id, name, src.ip(), tcp, None);
                send_packet(&sock, port, &here_packet(&shared)).await;
            }
            Packet::Here { id, name, port: tcp } => {
                if id == shared.id {
                    continue;
                }
                let rtt = (*shared.last_probe_at.lock().unwrap())
                    .filter(|t| t.elapsed() < Duration::from_secs(2))
                    .map(|t| t.elapsed().as_micros() as u64);
                upsert_peer(&shared, id, name, src.ip(), tcp, rtt);
            }
            Packet::Bye { id } => {
                shared.peers.lock().unwrap().remove(&id);
            }
        }
    }
}

/// Keep the peer table warm: announce presence periodically, prune the stale.
pub async fn announce_loop(shared: Arc<Shared>, sock: Arc<UdpSocket>, port: u16) {
    // Initial probe so existing peers answer immediately and we start with a
    // populated table instead of waiting for their next announce.
    send_probe(&shared, &sock, port).await;
    loop {
        tokio::time::sleep(ANNOUNCE_EVERY).await;
        send_packet(&sock, port, &here_packet(&shared)).await;
        shared
            .peers
            .lock()
            .unwrap()
            .retain(|_, p| p.last_seen.elapsed() < PEER_STALE_AFTER);
    }
}

pub async fn send_probe(shared: &Shared, sock: &UdpSocket, port: u16) {
    *shared.last_probe_at.lock().unwrap() = Some(Instant::now());
    let p = Packet::Probe {
        id: shared.id.clone(),
        name: shared.name.clone(),
        port: shared.tcp_port,
    };
    send_packet(sock, port, &p).await;
}

/// Active discovery: probe burst, then return as soon as replies go quiet.
/// `wait_ms` is a ceiling, not a fixed wait — on a quiet LAN this completes
/// in ~150ms. Returns the number of known peers.
pub async fn discover(shared: &Arc<Shared>, sock: &Arc<UdpSocket>, port: u16, wait_ms: u64) -> usize {
    let start = Instant::now();
    let deadline = Duration::from_millis(wait_ms.clamp(50, 5000));
    let quiet = Duration::from_millis(130);

    send_probe(shared, sock, port).await;
    let mut last_count = shared.peers.lock().unwrap().len();
    let mut last_change = Instant::now();
    let mut resent = false;

    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let count = shared.peers.lock().unwrap().len();
        if count != last_count {
            last_count = count;
            last_change = Instant::now();
        }
        let elapsed = start.elapsed();
        // One re-burst at 60ms covers single-packet UDP loss cheaply.
        if !resent && elapsed >= Duration::from_millis(60) {
            send_probe(shared, sock, port).await;
            resent = true;
        }
        if elapsed >= deadline {
            break;
        }
        if elapsed >= Duration::from_millis(120) && last_change.elapsed() >= quiet {
            break;
        }
    }
    last_count
}

pub async fn send_bye(shared: &Shared, sock: &UdpSocket, port: u16) {
    send_packet(sock, port, &Packet::Bye { id: shared.id.clone() }).await;
}

#[derive(Serialize, Deserialize)]
struct WireMsg {
    from_id: String,
    from_name: String,
    body: String,
}

/// Accept peer messages: one JSON line in, one ack line out.
pub async fn inbox_listener(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        let Ok((stream, addr)) = listener.accept().await else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            let read = tokio::time::timeout(
                Duration::from_secs(5),
                BufReader::new(r).read_line(&mut line),
            )
            .await;
            if !matches!(read, Ok(Ok(n)) if n > 0) {
                return;
            }
            let Ok(msg) = serde_json::from_str::<WireMsg>(line.trim()) else {
                return;
            };
            shared.inbox.lock().unwrap().push(InboxMsg {
                from_id: msg.from_id,
                from_name: msg.from_name,
                from_addr: addr.ip(),
                body: msg.body,
                received_unix_ms: unix_ms(),
            });
            shared.inbox_notify.notify_waiters();
            let _ = w.write_all(b"{\"ok\":true}\n").await;
        });
    }
}

/// Deliver one message to a peer over TCP and wait for its ack.
/// Returns the round-trip time in microseconds.
pub async fn send_message(shared: &Shared, peer: SocketAddr, body: &str) -> Result<u64, String> {
    let started = Instant::now();
    let stream = tokio::time::timeout(Duration::from_millis(1500), TcpStream::connect(peer))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
    let (r, mut w) = stream.into_split();

    let wire = serde_json::json!({
        "from_id": shared.id,
        "from_name": shared.name,
        "body": body,
    });
    let mut data = serde_json::to_vec(&wire).unwrap();
    data.push(b'\n');
    w.write_all(&data)
        .await
        .map_err(|e| format!("send failed: {e}"))?;

    let mut ack = String::new();
    let read = tokio::time::timeout(
        Duration::from_millis(2000),
        BufReader::new(r).read_line(&mut ack),
    )
    .await;
    match read {
        Ok(Ok(n)) if n > 0 => Ok(started.elapsed().as_micros() as u64),
        Ok(Ok(_)) => Err("peer closed connection without ack".into()),
        Ok(Err(e)) => Err(format!("ack read failed: {e}")),
        Err(_) => Err("ack timeout".into()),
    }
}

/// Best-effort local IP: the interface the kernel would route multicast through.
/// No packets are sent; works offline.
pub fn local_ip(port: u16) -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((MCAST_GROUP, port)).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}
