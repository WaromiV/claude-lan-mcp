use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub addr: IpAddr,
    pub tcp_port: u16,
    pub last_seen: Instant,
    pub rtt_micros: Option<u64>,
}

pub struct InboxMsg {
    pub from_id: String,
    pub from_name: String,
    pub from_addr: IpAddr,
    pub body: String,
    pub received_unix_ms: u64,
}

pub struct Shared {
    pub id: String,
    pub name: String,
    pub tcp_port: u16,
    pub peers: Mutex<HashMap<String, PeerInfo>>,
    pub inbox: Mutex<Vec<InboxMsg>>,
    pub inbox_notify: Notify,
    /// When the most recent active probe was sent; used to attribute RTT to replies.
    pub last_probe_at: Mutex<Option<Instant>>,
}

impl Shared {
    pub fn new(tcp_port: u16) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let seed = nanos ^ ((std::process::id() as u64) << 32);
        let id = format!("{:08x}", seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32);

        let name = std::env::var("CLAUDE_LAN_NAME").unwrap_or_else(|_| {
            let host = gethostname::gethostname().to_string_lossy().into_owned();
            format!("{}-{}", host, &id[..4])
        });

        Shared {
            id,
            name,
            tcp_port,
            peers: Mutex::new(HashMap::new()),
            inbox: Mutex::new(Vec::new()),
            inbox_notify: Notify::new(),
            last_probe_at: Mutex::new(None),
        }
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
