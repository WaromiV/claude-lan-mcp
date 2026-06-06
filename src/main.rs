mod mcp;
mod net;
mod state;

use state::Shared;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let mcast_port = net::mcast_port();

    // Per-instance TCP inbox on an ephemeral port, advertised via discovery.
    let tcp = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("failed to bind tcp inbox");
    let tcp_port = tcp.local_addr().unwrap().port();

    let shared = Arc::new(Shared::new(tcp_port));
    let udp = Arc::new(net::bind_discovery(mcast_port).expect("failed to bind discovery socket"));

    eprintln!(
        "[claude-lan-mcp] up as '{}' (id {}) — inbox tcp/{}, discovery udp/{}",
        shared.name, shared.id, tcp_port, mcast_port
    );

    tokio::spawn(net::discovery_listener(shared.clone(), udp.clone(), mcast_port));
    tokio::spawn(net::announce_loop(shared.clone(), udp.clone(), mcast_port));
    tokio::spawn(net::inbox_listener(shared.clone(), tcp));

    // Serve MCP over stdio; returns when the client closes stdin.
    mcp::serve(shared.clone(), udp.clone(), mcast_port).await;

    // Tell the LAN we're gone so peers drop us immediately.
    net::send_bye(&shared, &udp, mcast_port).await;
}
