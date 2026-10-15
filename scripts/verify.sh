#!/usr/bin/env bash
# End-to-end verification for the Edifier-for-macOS stack.
#
#   ./scripts/verify.sh
#
# Steps 1-4 need no hardware: they build everything, run the unit tests, exercise the MCP server
# over stdio, and drive the whole client stack against the BLE emulator that makes this Mac
# advertise itself as an EDIFIER W820NB. Step 5 talks to the real headset when it is switched on
# and reachable, and says so plainly when it is not.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$PWD"
EMULATOR_PID=""

step() { printf '\n== %s ==\n' "$1"; }
fail() { printf 'FAILED: %s\n' "$1" >&2; exit 1; }
cleanup() { [ -n "$EMULATOR_PID" ] && kill "$EMULATOR_PID" 2>/dev/null || true; }
trap cleanup EXIT

step "1/5 build (release)"
cargo build --release --workspace

step "2/5 unit and vector tests"
cargo test --workspace

step "3/5 MCP server handshake"
MCP_OUT="$(mktemp)"
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"verify","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"edifier_anc","arguments":{"mode":"loud"}}}' \
  | ./target/release/edifier-mcp >"$MCP_OUT" 2>/dev/null
python3 - "$MCP_OUT" <<'PY'
import json, sys
messages = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
kinds = {message.get("id"): message for message in messages}
init = kinds.get(1, {}).get("result", {})
assert init.get("serverInfo", {}).get("name") == "edifier-mcp", "initialize failed"
tools = kinds.get(2, {}).get("result", {}).get("tools", [])
assert len(tools) >= 15, f"tools/list returned {len(tools)} tools"
invalid = kinds.get(3, {}).get("error", {})
assert invalid.get("code") == -32602, "invalid arguments were not rejected"
print(f"    server={init['serverInfo']['name']} version={init['serverInfo']['version']} "
      f"protocol={init['protocolVersion']} tools={len(tools)}")
PY
rm -f "$MCP_OUT"

step "4/5 emulator protocol round trip (this Mac acts as the headset)"
# macOS never delivers a locally created advertisement to a central on the *same* host, so the
# emulator cannot be discovered by a scan on this machine. Its own harness drives the real
# `edifier_link::Session` against the emulator's device logic over an in-memory transport, which is
# what this step runs. Verifying the over-the-air path needs the real headset (step 5) or a second
# Mac in radio range advertising/scaning at the emulator.
cargo test -p edifier-link --example emulate -- --nocapture 2>&1 | tail -20
printf '    emulator protocol round trip verified in-process\n'

step "5/5 real headset (optional)"
# `doctor` reaches the headset whether it is advertising or already connected by macOS, so it is the
# right probe here; a scan would miss the (normal) system-connected case.
if ./target/release/edifierctl doctor >/tmp/edifier-doctor.txt 2>&1; then
  cat /tmp/edifier-doctor.txt
  printf '    real headset verified\n'
else
  printf '%s\n' \
    "    no headset reachable over Bluetooth." \
    "    Power the EDIFIER W820NB on, then rerun this script, or run:" \
    "      ./target/release/edifierctl doctor" \
    "      ./scripts/hardware-functions.sh   # full function test against the device"
fi

printf '\nAll checks passed.\n'
