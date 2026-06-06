#!/usr/bin/env bash
# Smoke test: spin up two instances, verify discovery + message delivery.
set -uo pipefail
BIN=${1:-target/release/claude-lan-mcp}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
INITED='{"jsonrpc":"2.0","method":"notifications/initialized"}'

# beta: come up, long-poll inbox for 4s
{
  printf '%s\n%s\n' "$INIT" "$INITED"
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lan_inbox","arguments":{"wait_ms":4000}}}'
  sleep 4.5
} | CLAUDE_LAN_NAME=beta "$BIN" >"$TMP/b.out" 2>"$TMP/b.err" &
B=$!

sleep 0.4

# alpha: discover, then send to beta
{
  printf '%s\n%s\n' "$INIT" "$INITED"
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lan_discover","arguments":{"wait_ms":600}}}'
  sleep 1
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lan_send","arguments":{"to":"beta","message":"hello from alpha"}}}'
  sleep 1
} | CLAUDE_LAN_NAME=alpha "$BIN" >"$TMP/a.out" 2>"$TMP/a.err" &
A=$!

wait "$A" "$B" 2>/dev/null

echo "--- alpha ---"; cat "$TMP/a.out"
echo "--- beta ---";  cat "$TMP/b.out"

fail=0
grep -qF 'beta'                "$TMP/a.out" || { echo "FAIL: alpha did not discover beta"; fail=1; }
grep -qF '\"delivered\":true'  "$TMP/a.out" || { echo "FAIL: send was not delivered";      fail=1; }
grep -qF 'hello from alpha'    "$TMP/b.out" || { echo "FAIL: beta did not receive msg";    fail=1; }
[ "$fail" = 0 ] && echo "SMOKE OK"
exit "$fail"
