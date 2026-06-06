use crate::net;
use crate::state::Shared;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UdpSocket;

/// MCP over stdio: newline-delimited JSON-RPC 2.0. Returns on stdin EOF.
pub async fn serve(shared: Arc<Shared>, udp: Arc<UdpSocket>, mcast_port: u16) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = req.get("id").cloned().filter(|v| !v.is_null());
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications get no response.
        let Some(id) = id else { continue };

        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => handle_call(&shared, &udp, mcast_port, &params).await,
            other => Err((-32601, format!("method not found: {other}"))),
        };

        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let mut out = serde_json::to_vec(&resp).unwrap();
        out.push(b'\n');
        if stdout.write_all(&out).await.is_err() {
            return;
        }
        let _ = stdout.flush().await;
    }
}

fn initialize_result(params: &Value) -> Value {
    let proto = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("2025-06-18");
    json!({
        "protocolVersion": proto,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "claude-lan-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "lan_whoami",
            "description": "This node's identity on the LAN: name, id, ip, and ports. Other Claude instances address you by this name.",
            "inputSchema": {"type": "object", "properties": {}, "required": []}
        },
        {
            "name": "lan_discover",
            "description": "Actively probe the LAN for other Claude instances (multicast + broadcast burst). Returns as soon as replies go quiet — typically under 200ms. Returns the full peer table with round-trip latencies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wait_ms": {"type": "integer", "description": "Ceiling on how long to wait for replies, in ms (default 300, max 5000). Early-exits when the LAN goes quiet."}
                },
                "required": []
            }
        },
        {
            "name": "lan_peers",
            "description": "Instantly list peers already known from background discovery, without sending any probes. Use lan_discover for a fresh active scan.",
            "inputSchema": {"type": "object", "properties": {}, "required": []}
        },
        {
            "name": "lan_send",
            "description": "Send a text message to another Claude instance by name or id, or to 'all' to broadcast to every known peer. Auto-discovers first if the peer isn't known yet. Delivery is acknowledged over TCP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Peer name, peer id, a unique name prefix, or 'all'"},
                    "message": {"type": "string", "description": "The message text to deliver"}
                },
                "required": ["to", "message"]
            }
        },
        {
            "name": "lan_inbox",
            "description": "Read and clear messages received from other Claude instances. Set wait_ms to long-poll: blocks until a message arrives or the timeout passes — use this to await a reply in a conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wait_ms": {"type": "integer", "description": "If the inbox is empty, wait up to this many ms for a message to arrive (default 0 = return immediately, max 25000)"}
                },
                "required": []
            }
        }
    ])
}

async fn handle_call(
    shared: &Arc<Shared>,
    udp: &Arc<UdpSocket>,
    mcast_port: u16,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let outcome: Result<Value, String> = match name {
        "lan_whoami" => Ok(whoami(shared, mcast_port)),
        "lan_discover" => {
            let wait = args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(300);
            net::discover(shared, udp, mcast_port, wait).await;
            Ok(peers_json(shared))
        }
        "lan_peers" => Ok(peers_json(shared)),
        "lan_send" => send_tool(shared, udp, mcast_port, &args).await,
        "lan_inbox" => Ok(inbox_tool(shared, &args).await),
        other => Err(format!("unknown tool: {other}")),
    };

    Ok(match outcome {
        Ok(v) => json!({
            "content": [{"type": "text", "text": v.to_string()}],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{"type": "text", "text": e}],
            "isError": true
        }),
    })
}

fn whoami(shared: &Shared, mcast_port: u16) -> Value {
    json!({
        "id": shared.id,
        "name": shared.name,
        "ip": net::local_ip(mcast_port).map(|i| i.to_string()),
        "tcp_port": shared.tcp_port,
        "discovery_port": mcast_port,
        "multicast_group": net::MCAST_GROUP.to_string(),
    })
}

fn peers_json(shared: &Shared) -> Value {
    let peers = shared.peers.lock().unwrap();
    let mut list: Vec<Value> = peers
        .values()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "ip": p.addr.to_string(),
                "tcp_port": p.tcp_port,
                "rtt_ms": p.rtt_micros.map(|u| (u as f64 / 100.0).round() / 10.0),
                "last_seen_secs_ago": (p.last_seen.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            })
        })
        .collect();
    list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({"count": list.len(), "peers": list})
}

fn resolve(shared: &Shared, to: &str) -> Vec<(String, SocketAddr)> {
    let peers = shared.peers.lock().unwrap();
    if to == "all" || to == "*" {
        return peers
            .values()
            .map(|p| (p.name.clone(), SocketAddr::new(p.addr, p.tcp_port)))
            .collect();
    }
    if let Some(p) = peers.values().find(|p| p.id == to || p.name == to) {
        return vec![(p.name.clone(), SocketAddr::new(p.addr, p.tcp_port))];
    }
    // Fall back to a unique name prefix so "alpha" matches "alpha-3f2c".
    let matches: Vec<_> = peers
        .values()
        .filter(|p| p.name.starts_with(to))
        .collect();
    if matches.len() == 1 {
        let p = matches[0];
        return vec![(p.name.clone(), SocketAddr::new(p.addr, p.tcp_port))];
    }
    Vec::new()
}

async fn send_tool(
    shared: &Arc<Shared>,
    udp: &Arc<UdpSocket>,
    mcast_port: u16,
    args: &Value,
) -> Result<Value, String> {
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument 'to'")?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument 'message'")?;

    let mut targets = resolve(shared, to);
    if targets.is_empty() {
        // Peer table may be cold (e.g. first call) — one quick active scan.
        net::discover(shared, udp, mcast_port, 350).await;
        targets = resolve(shared, to);
    }
    if targets.is_empty() {
        return Err(format!(
            "no peer matching '{to}' found on the LAN (try lan_discover)"
        ));
    }

    let mut results = Vec::new();
    for (peer_name, addr) in targets {
        let entry = match net::send_message(shared, addr, message).await {
            Ok(micros) => json!({
                "peer": peer_name,
                "delivered": true,
                "rtt_ms": (micros as f64 / 100.0).round() / 10.0,
            }),
            Err(e) => json!({"peer": peer_name, "delivered": false, "error": e}),
        };
        results.push(entry);
    }
    Ok(json!({"results": results}))
}

async fn inbox_tool(shared: &Arc<Shared>, args: &Value) -> Value {
    let wait_ms = args
        .get("wait_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(25_000);

    let drain = |shared: &Shared| -> Vec<Value> {
        shared
            .inbox
            .lock()
            .unwrap()
            .drain(..)
            .map(|m| {
                json!({
                    "from": m.from_name,
                    "from_id": m.from_id,
                    "from_ip": m.from_addr.to_string(),
                    "body": m.body,
                    "received_unix_ms": m.received_unix_ms,
                })
            })
            .collect()
    };

    // Register interest before the first drain so a message landing between
    // drain and await still wakes us.
    let mut notified = std::pin::pin!(shared.inbox_notify.notified());
    notified.as_mut().enable();

    let mut msgs = drain(shared);
    if msgs.is_empty() && wait_ms > 0 {
        let _ = tokio::time::timeout(Duration::from_millis(wait_ms), notified).await;
        msgs = drain(shared);
    }
    json!({"count": msgs.len(), "messages": msgs})
}
