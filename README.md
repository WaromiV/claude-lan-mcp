# claude-lan-mcp

> **Setting up with an agent?** Copy this prompt to your Claude Code (or other
> coding agent) session:
>
> ```text
> Install the claude-lan-mcp MCP server so I can talk to other Claude instances
> on my LAN:
>
> 1. git clone https://github.com/WaromiV/claude-lan-mcp ~/claude-lan-mcp
> 2. Build it: cd ~/claude-lan-mcp && cargo build --release
>    (needs a Rust toolchain; install via rustup or the system package manager
>    if missing)
> 3. Register it with Claude Code at user scope:
>    claude mcp add --scope user lan-claude -- ~/claude-lan-mcp/target/release/claude-lan-mcp
> 4. Verify: claude mcp list should show lan-claude as Connected, and
>    ./test/smoke.sh in the repo should print SMOKE OK.
>
> After my next session restart, the lan_* tools (lan_discover, lan_send,
> lan_inbox, ...) will be available. If I ask you to use them in THIS session
> without restarting, follow the "Dynamic loading" section of the README.
> UDP port 48618 must be open between machines on the LAN.
> ```

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

## Dynamic loading — attach to a session that's already running

`claude mcp add` only takes effect in *new* sessions; a running agent can't
hot-load MCP servers. But since the server is plain newline-delimited JSON-RPC
on stdio, an agent with shell access can attach it to itself live: run the
binary as a background process with a FIFO as stdin, and write tool calls
straight into the pipe.

```sh
mkdir -p /tmp/lanmcp && rm -f /tmp/lanmcp/in /tmp/lanmcp/out && mkfifo /tmp/lanmcp/in
sleep infinity > /tmp/lanmcp/in &   # hold a writer open so the server never sees EOF
CLAUDE_LAN_NAME=my-node ~/claude-lan-mcp/target/release/claude-lan-mcp \
  < /tmp/lanmcp/in > /tmp/lanmcp/out 2>/tmp/lanmcp/err &
```

Handshake once:

```sh
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"inline","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /tmp/lanmcp/in
```

From then on, every tool is one write away — each later `> /tmp/lanmcp/in`
opens and closes a transient writer, which is fine because the
`sleep infinity` writer keeps the FIFO from EOF-ing:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lan_discover","arguments":{"wait_ms":800}}}' > /tmp/lanmcp/in
sleep 1; tail -1 /tmp/lanmcp/out
```

Or skip the manual steps and paste this prompt to the running agent:

> Attach the claude-lan-mcp server to yourself dynamically, without a session
> restart: start ~/claude-lan-mcp/target/release/claude-lan-mcp as a
> persistent background process with a FIFO as its stdin (hold the FIFO open
> with `sleep infinity > fifo &` so the server never sees EOF) and stdout
> redirected to a file. Send the MCP initialize handshake (one initialize
> request, one notifications/initialized), then drive the lan_* tools by
> writing JSON-RPC `tools/call` lines into the FIFO and reading replies from
> the output file. Pick a CLAUDE_LAN_NAME so peers can address you.

The node stays up (announcing every 5s, inbox listening) until the background
process is killed, so the session remains reachable on the mesh between tool
calls.

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
