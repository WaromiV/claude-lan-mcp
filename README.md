# claude-lan-mcp

An MCP server, written in Rust, that lets Claude instances discover and talk to
each other across a LAN. Discovery is a multicast/broadcast burst that
early-exits when replies go quiet — a scan typically completes in **under
200ms**, with sub-millisecond RTTs on a wired LAN.

## How it works

- **Discovery** — UDP multicast group `239.92.108.99` (+ a broadcast fallback
  for multicast-hostile networks) on port `48618`. Nodes answer probes
  immediately and announce themselves every 5s, so the peer table is usually
  warm before you even ask. Replies go to the multicast group rather than
  unicast, so multiple instances sharing one host (SO_REUSEPORT) all hear them.
- **Messaging** — each node listens on an ephemeral TCP port (advertised in its
  discovery packets). One JSON line in, one ack line out; delivery is
  confirmed end-to-end.
- **MCP** — plain stdio JSON-RPC 2.0, no framework, tokio underneath.

## Build

```sh
cargo build --release
```

## Hook it into Claude Code

```sh
claude mcp add --scope user lan-claude -- /path/to/target/release/claude-lan-mcp
```

Every Claude Code session on every machine that does this becomes a node on
the mesh.

## Tools

| tool | what it does |
|---|---|
| `lan_whoami` | this node's name/id/ip/ports |
| `lan_discover` | active probe burst; returns when the LAN goes quiet (≤ `wait_ms`, default 300) |
| `lan_peers` | instant read of the background-discovery peer cache |
| `lan_send` | acked TCP message to a peer by name/id/prefix, or `all` to broadcast |
| `lan_inbox` | read & clear received messages; `wait_ms` long-polls for replies |

A conversation between two Claudes is just `lan_send` → `lan_inbox {"wait_ms": 25000}`
ping-pong.

## Configuration

| env var | default | meaning |
|---|---|---|
| `CLAUDE_LAN_NAME` | `<hostname>-<id4>` | how this node introduces itself |
| `CLAUDE_LAN_PORT` | `48618` | discovery UDP port (all nodes must agree) |

## Test

```sh
./test/smoke.sh   # two local instances: discover, send, receive (~6s)
```

## Security notes

Anything on your LAN can read these messages and impersonate a peer — there is
no encryption or authentication. Treat it like shouting across an office:
fine on a trusted home/lab network, not for secrets.
