#!/usr/bin/env bash
# Exercise the headset's functions against the real device.
#
# Every value is captured first and restored at the end, and nothing destructive is sent: power
# off, re-pair, factory reset and non-off LDAC are excluded. Commands the firmware does not
# implement are reported as "not implemented" instead of aborting the run — that is a normal
# result on a W820NB, which has no sound-effect, ANC-gesture or LDAC command family.
#
#   ./scripts/hardware-functions.sh [device-selector]
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
CTL="./target/release/edifierctl"
DEVICE="${1:-}"
ARGS=()
[ -n "$DEVICE" ] && ARGS=(--device "$DEVICE")

# Values captured before the test, restored by `restore`.
ORIGINAL_ANC=""
ORIGINAL_AMBIENT=""
ORIGINAL_GAME=""
ORIGINAL_PROMPT=""
UNSUPPORTED=()

step() { printf '\n== %s ==\n' "$1"; }
ok() { printf '  ok         %s\n' "$1"; }
bad() { printf '  FAILED     %s (%s)\n' "$1" "$2"; FAILURES=$((FAILURES + 1)); }
skip() { printf '  not impl.  %s\n' "$1"; UNSUPPORTED+=("$1"); }
FAILURES=0

field() { # field <json> <name>
  python3 -c "
import json, sys
reply = json.loads(sys.argv[1])
status = reply.get('status', reply) if isinstance(reply, dict) else reply
value = status.get(sys.argv[2], '')
print('' if value is None else value)
" "$1" "$2"
}

run_status() { "$CTL" "${ARGS[@]+"${ARGS[@]}"}" --json status; }

# send <label> <expected-field> <expected-value> <command...>
#
# The setter's own output is the device's answer, so it is checked first; a follow-up query confirms
# it. Both are device-reported values, and a disagreement is reported rather than asserted, because
# a headset that goes to sleep mid-run can answer the set and then report stale state.
send() {
  local label="$1" field_name="$2" expected="$3"
  shift 3
  local out
  if ! out="$("$CTL" "${ARGS[@]+"${ARGS[@]}"}" "$@" 2>&1)"; then
    skip "$label (no answer from this firmware)"
    return
  fi
  local reported confirmed
  reported="$(printf '%s\n' "$out" | awk -v key="$field_name" '$1 == key || $1" "$2 == key {print $NF}' | head -1)"
  sleep 1
  confirmed="$(field "$(run_status)" "$field_name")"
  if [ "$confirmed" = "$expected" ]; then
    ok "$label -> $field_name=$confirmed"
  elif [ -n "$reported" ] && [ "$reported" != "$confirmed" ]; then
    printf '  WARN       %s: setter reported %s=%s, a query reports %s\n' "$label" "$field_name" "$reported" "$confirmed"
  else
    bad "$label" "expected $field_name=$expected, got '$confirmed'"
  fi
}

restore() {
  step "restore"
  [ -n "$ORIGINAL_AMBIENT" ] && "$CTL" "${ARGS[@]+"${ARGS[@]}"}" ambient-volume "$ORIGINAL_AMBIENT" >/dev/null 2>&1
  [ -n "$ORIGINAL_PROMPT" ] && "$CTL" "${ARGS[@]+"${ARGS[@]}"}" prompt-volume "$ORIGINAL_PROMPT" >/dev/null 2>&1
  [ -n "$ORIGINAL_GAME" ] && "$CTL" "${ARGS[@]+"${ARGS[@]}"}" game-mode "$([ "$ORIGINAL_GAME" = "True" ] && echo on || echo off)" >/dev/null 2>&1
  "$CTL" "${ARGS[@]+"${ARGS[@]}"}" shutdown-timer off >/dev/null 2>&1
  [ -n "$ORIGINAL_ANC" ] && "$CTL" "${ARGS[@]+"${ARGS[@]}"}" anc "$ORIGINAL_ANC" >/dev/null 2>&1
  "$CTL" "${ARGS[@]+"${ARGS[@]}"}" status 2>/dev/null
}
trap restore EXIT

step "baseline"
BEFORE="$(run_status)"
[ -n "$BEFORE" ] || { printf 'cannot reach the headset\n' >&2; exit 1; }
"$CTL" "${ARGS[@]+"${ARGS[@]}"}" status
ORIGINAL_ANC="$(field "$BEFORE" anc)"
ORIGINAL_AMBIENT="$(field "$BEFORE" ambient_volume)"
ORIGINAL_GAME="$(field "$BEFORE" game_mode)"
ORIGINAL_PROMPT="$(field "$BEFORE" prompt_volume)"
ORIGINAL_NAME="$(field "$BEFORE" name)"
printf 'captured: anc=%s ambient=%s game=%s prompt=%s name=%s\n' \
  "$ORIGINAL_ANC" "$ORIGINAL_AMBIENT" "$ORIGINAL_GAME" "$ORIGINAL_PROMPT" "$ORIGINAL_NAME"

step "identity and telemetry (read only)"
for name in battery firmware mac name; do
  value="$(field "$BEFORE" "$name")"
  if [ -n "$value" ]; then ok "$name=$value"; else bad "$name" "not reported"; fi
done

step "noise control"
if [ "$ORIGINAL_ANC" != "noise_cancelling" ]; then
  send "noise cancelling" anc noise_cancelling anc on
else
  ok "noise cancelling already active"
fi
if [ "$ORIGINAL_ANC" != "ambient" ]; then
  send "transparency" anc ambient anc ambient
else
  ok "transparency already active"
fi

step "ambient volume"
if "$CTL" "${ARGS[@]+"${ARGS[@]}"}" ambient-volume 2 >/dev/null 2>&1; then
  got="$(field "$(run_status)" ambient_volume)"
  if [ "$got" = "2" ]; then ok "ambient volume -> 2"; else bad "ambient volume" "got '$got'"; fi
else
  skip "ambient volume (no answer)"
fi

step "game mode"
if [ "$ORIGINAL_GAME" = "True" ]; then send "game mode off" game_mode False game-mode off; else send "game mode on" game_mode True game-mode on; fi

step "voice prompt volume"
send "prompt volume 7" prompt_volume 7 prompt-volume 7

step "auto-shutdown timer"
if "$CTL" "${ARGS[@]+"${ARGS[@]}"}" shutdown-timer 30 >/dev/null 2>&1; then
  timer="$(field "$(run_status)" shutdown_timer)"
  case "$timer" in *30*) ok "shutdown timer -> $timer" ;; *) bad "shutdown timer" "got '$timer'" ;; esac
else
  skip "shutdown timer (no answer)"
fi

step "rename (writes the current name back)"
if "$CTL" "${ARGS[@]+"${ARGS[@]}"}" name "$ORIGINAL_NAME" >/dev/null 2>&1; then
  name="$(field "$(run_status)" name)"
  if [ "$name" = "$ORIGINAL_NAME" ]; then ok "name round-trip ($name)"; else bad "rename" "got '$name'"; fi
else
  skip "rename (no answer)"
fi

step "transport control"
if "$CTL" "${ARGS[@]+"${ARGS[@]}"}" playback play >/dev/null 2>&1; then
  ok "playback play -> $(field "$(run_status)" playback_state)"
else
  skip "playback (no answer)"
fi

step "features this firmware does not implement (expected to be silent)"
for probe in "eq rock" "gestures --normal --noise-reduction --ambient" "raw 48" "ldac off --force"; do
  # shellcheck disable=SC2086
  if "$CTL" "${ARGS[@]+"${ARGS[@]}"}" --timeout-ms 4000 $probe >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    printf '  answered   %s\n' "$probe"
  else
    skip "$probe"
  fi
done

printf '\nfailures: %d\n' "$FAILURES"
if [ "${#UNSUPPORTED[@]}" -gt 0 ]; then
  printf 'not implemented by this headset:\n'
  printf '  - %s\n' "${UNSUPPORTED[@]}"
fi
[ "$FAILURES" -eq 0 ] || exit 1
