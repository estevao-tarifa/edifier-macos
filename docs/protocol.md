# Edifier control protocol (reverse engineered)

Everything here comes from the Edifier Connect traffic captured by
[mEDIFIER](https://github.com/wh201906/mEDIFIER) and is re-verified against
`crates/edifier-proto/tests/vectors.rs`. Upstream sources: `Qt/comms/comm.cpp` (framing),
`Qt/devices/basedevice.cpp` (commands), `doc/KnownCommands/W820NB_Double_Gold.txt` (captures),
`Qt/devices/deviceinfo.json` (per-model feature matrix). No behaviour is documented by the vendor.

## Framing

```text
host -> device:  AA | len | payload[len] | sum_hi | sum_lo
device -> host:  BB | len | payload[len] | sum_hi | sum_lo     state / value response
device -> host:  CC | len | payload[len] | sum_hi | sum_lo     acknowledgement
```

- `len` counts **payload** bytes only; it excludes the header byte and the 2-byte checksum.
- Payload byte 0 is the command; the remaining bytes are parameters.
- Checksum: `0x2019` plus every byte of the frame *so far* (header byte and length byte included),
  added with 16-bit wraparound and written big-endian (mEDIFIER's `Comm::addChecksum` seeds
  `quint16 sum = 8217` = `0x2019`).

Worked example, the first `sendData:` line of the capture:

```text
AA 02 C1 01        -> 0x2019 + 0xAA + 0x02 + 0xC1 + 0x01
                   -> 8217 + 170 + 2 + 193 + 1 = 8583 = 0x2187
                   -> AA 02 C1 01 21 87      (the capture's "AA02C1012187")
```

The reply follows the same rule over its own bytes; `BB 03 C1 01 06` (noise cancelling, ambient
volume 0) gives `0x2019 + 0xBB + 0x03 + 0xC1 + 0x01 + 0x06 = 8607 = 0x219F`.

Reply shapes: `BB` is a state or value (`BB 03 CC <mode> <byte>` the ANC mode and ambient-sound
byte, `BB 02 D0 <percent>` one byte, `BB 07 C8 <6 bytes>` the MAC); `CC` acknowledges a request and
echoes it as `CC <len> <command> <status> …`, e.g. `CC 02 CA 01` (`CC02CA0121B2`) for a rename.

The payload length is one byte, so payloads are capped at 255 bytes (`FrameError::PayloadTooLong`).
`FrameDecoder` consumes a byte stream: it discards bytes that cannot start a frame, buffers an
incomplete frame until its length is satisfied, and resets the buffer after 5 s of silence
(`packetTimeoutMs` in mEDIFIER's `comm.cpp`).

## Transport (BLE)

The headset exposes one primary service in the `4809xxxx-1a48-11e9-ab14-d663bd873d93` family
(`edifier-link` also accepts the W800K's `00001000-0000-1000-8991-00805f9b34fb`). Inside that
service the control pair is the notify characteristic `48090001-…` plus the write characteristic
`48090002-…`, or for a model-specific service the characteristic whose first group ends in `2`
(notify) / `3` (write). Host frames go out in 20-byte chunks with a 50 ms gap (`maxPacketLen = 20`,
`packetDelayMs = 50` in mEDIFIER's `commble.h`); notifications arrive as they are and may split a
frame anywhere.

This control link is independent of the Bluetooth **audio** link: the headset can stream from a
phone while the Mac still controls it over BLE, which is why the menu bar app and MCP server keep
working when the audio link is elsewhere, and why "move the audio link back to this Mac" is an
IOBluetooth operation (`edifier-macos`, `connect-audio`) rather than a protocol command.

## Command table

Each row lists the payload bytes, the expected reply, and the CLI subcommand and MCP tool that send it. The queries form the `status`/`doctor` refresh burst (`edifier_status`) and are streamed by `monitor`.

| Payload (hex) | Meaning | Reply | CLI | MCP |
| --- | --- | --- | --- | --- |
| `CC` | Query ANC mode + ambient volume | `BB 03 CC <mode> <byte>` | `status` | `edifier_status` |
| `C1 01` / `C1 02` / `C1 03` | Set ANC: normal / noise cancelling / ambient | `BB 03 C1 <mode> <byte>` | `anc off\|on\|ambient` | `edifier_anc` |
| `C1 03 <byte>` | Set ambient volume, `byte = volume + 6`, volume -3..=3 | `BB 03 C1 03 <byte>` | `ambient-volume <level>` | `edifier_ambient_volume` |
| `D5` | Query sound-effect preset | `BB 02 D5 <code>` | `status` | `edifier_status` |
| `C4 00..03` | Set sound effect: normal / pop / classical / rock | `CC 02 C4 01` ack | `eq <preset>` | `edifier_eq` |
| `F0 0A` | Query ANC gesture mask | `BB 03 F0 0A <mask>` | `status` | `edifier_status` |
| `F1 0A <mask>` | Set ANC gesture mask (bit 0 normal, bit 1 noise reduction, bit 2 ambient) | `BB 03 F1 0A <mask>` | `gestures` | `edifier_gestures` |
| `48` | Query LDAC mode | `BB 02 48 <mode>` | `status` | `edifier_status` |
| `49 00` / `49 01` / `49 02` | Set LDAC: off / 48 kHz / 96 kHz | `BB 02 49 <mode>` | `ldac off\|48k\|96k --force` | `edifier_ldac` |
| `08` | Query game (low-latency) mode | `BB 02 08 <flag>` | `status` | `edifier_status` |
| `09 00` / `09 01` | Set game mode off / on | `BB 02 09 <flag>` | `game-mode on\|off` | `edifier_game_mode` |
| `05` | Query prompt volume | `BB 02 05 <level>` | `status` | `edifier_status` |
| `06 <level>` | Set prompt volume, 0..=15 | `BB 02 06 <level>` | `prompt-volume <level>` | `edifier_prompt_volume` |
| `D3` | Query shutdown timer | `BB 02 D3 <flag>` or `BB 03 D3 00 <minutes>` | `status` | `edifier_status` |
| `D1 00 <minutes>` | Set shutdown timer, 0..=180 min (16-bit field, see below) | ack | `shutdown-timer <minutes>` | `edifier_shutdown_timer` |
| `D2` | Disable shutdown timer | `CC 02 D2 01` ack | `shutdown-timer off` | `edifier_shutdown_timer` (`minutes: null`) |
| `D7`† | Query auto power off when idle | `BB 02 D7 <flag>` | `status` | `edifier_status` |
| `D6 00` / `D6 01`† | Set auto power off off / on | `BB 02 D6 <flag>` | `raw "D6 01"` | `edifier_raw` |
| `D0` | Query battery | `BB 02 D0 <percent>` | `battery`, `status` | `edifier_status` |
| `C8` | Query MAC address | `BB 07 C8 <6 bytes>` | `mac`, `status` | `edifier_status` |
| `C6` | Query firmware version | `BB 04 C6 <3 bytes>` | `firmware`, `status` | `edifier_status` |
| `C9` | Query advertised name | `BB .. C9 <utf8>` | `name`, `status` | `edifier_status` |
| `CA <utf8>` | Set advertised name | `CC 02 CA 01` ack | `name <new-name>` | `edifier_rename` |
| `D8` | Query device fingerprint (20-byte blob) | `BB 16 D8 <20 bytes>` | `raw "D8"` | `edifier_raw` |
| `C3` | Query playback state | `BB 02 C3 <state>` | `raw "C3"` | `edifier_raw` |
| `C2 <00..05>`† | Playback action: play / pause / vol up / vol down / next / previous | ack | `playback <action>` | `edifier_playback` |
| `CD`† | Disconnect the current link | ack | `disconnect` | — (`edifier_disconnect` closes the BLE link instead) |
| `CE`† | Power off | ack | `power-off --yes` | `edifier_destructive` |
| `CF`† | Re-enter pairing mode | ack | `re-pair --yes` | `edifier_destructive` |
| `07`† | Factory reset | ack | `factory-reset --yes` | `edifier_destructive` |
| *any* | Raw payload escape hatch | device dependent | `raw <bytes…>` | `edifier_raw` |

† Not present in the W820NB Double Gold capture: the payload comes from mEDIFIER's `basedevice.cpp`
and the reply shape follows the patterns above. Every other row's reply is taken from a captured
`readData:` line.

Replies to `C1 0x` and `CC` echo both the mode and the ambient-sound byte, so a query and a set
settle the same state. Serialisation: the protocol carries no correlation id, so `edifier-link`
sends one request at a time and `refresh()` spaces its queries 150 ms apart.

## Model differences

Detection is by advertised service UUID first, then by name substring. Hidden features are exactly
the app's `HiddenFeatures` lists in mEDIFIER's `Qt/devices/deviceinfo.json`.

| Model | Service UUID | Max name | Hidden features |
| --- | --- | --- | --- |
| W820NB | `48093801-1a48-11e9-ab14-d663bd873d93` | 24 | sound-effect group, ANC-gesture group |
| W820NB Plus | treated as W820NB (untested upstream) | 24 | same as W820NB |
| W820NB Double Gold | `48097901-1a48-11e9-ab14-d663bd873d93` | 30 | auto-power-off toggle |
| W200BT Plus | `48092801-1a48-11e9-ab14-d663bd873d93` | 24 | noise group, ambient-sound group, ANC-gesture group, LDAC group, game mode |
| Generic / unknown | falls back to the W820NB UUID | 24 | none |

Hidden means the firmware does not implement the command: `refresh()` skips those queries, and a
timeout on an optional query is logged and skipped rather than fatal. Names longer than the model
limit are truncated on a UTF-8 character boundary before `CA` is sent.

## Quirks

**LDAC.** `49 01` / `49 02` put the headset into LDAC 48 kHz / 96 kHz, so it expects an LDAC
source from then on. macOS ships no LDAC A2DP encoder (evidence in
[macos-integration-limits.md](macos-integration-limits.md)), so audio from this Mac stops until the
mode is set back to `off` (`49 00`). The control link is independent of the audio link, so the
headset stays reachable and the change is reversible; the CLI and the MCP server demand `--force` /
`confirm=true` and refuse otherwise.

**Shutdown timer.** The timer is a 16-bit field on the wire: setting it sends `D1 00 <minutes>` —
the high byte is a real payload byte, not padding. Only 0..=180 is accepted
(`SHUTDOWN_TIMER_RANGE`), so the high byte is always `00` in practice. Both reply shapes are
decoded: `BB 02 D3 <flag>` (one byte, `0` = disabled, otherwise minutes) and
`BB 03 D3 00 <minutes>` (the 16-bit form). Clearing uses the single byte `D2`.

**Ambient volume.** The firmware stores `volume + 6`, so `-3..=3` maps to bytes `03..=09`, and
setting it implicitly switches the headset to ambient mode.

## Discovery on macOS

A headset that macOS already connected (`EDIFIER BLE`, `Services: 0x400000 < BLE >` in
`system_profiler SPBluetoothDataType`) no longer advertises, so a scan cannot see it. `edifier-link`
asks CoreBluetooth for the peripherals the system already holds for the control service
(`-[CBCentralManager retrieveConnectedPeripheralsWithServices:]`) and only scans when that list is
empty; both paths feed the same session. Capture evidence: with the headset connected, a scan listed
10-14 unrelated peripherals and never the headset, while the retrieved-connections query returned it
and every command was answered.

## Verifying

`cargo test -p edifier-proto` replays the capture without hardware:
`crates/edifier-proto/tests/vectors.rs` asserts every `sendData:` line of `doc/KnownCommands/W820NB_Double_Gold.txt` byte for byte, decodes the matching `readData:` lines into typed events, checks the accumulator/summary, and covers split frames, resynchronisation and checksum rejection.

To exercise the protocol without the physical headset, two paths exist:

```sh
cargo test -p edifier-proto                      # captured vectors (hardware-free)
cargo test -p edifier-link --example emulate     # emulator device logic driven by a real Session
```

`crates/edifier-link/examples/emulate.rs` makes this Mac advertise the control service and answer
the documented replies over real Bluetooth Low Energy (`cargo run -p edifier-link --example emulate`
prints `emulator-ready <identifier>` and a JSON state line per handled frame). One limitation is
inherent to the hardware: macOS never delivers a *locally created* advertisement to a central
manager on the same host, so a scan on this Mac cannot discover its own emulator — the emulator is
useful against a client on another machine, while the same-host round trip is covered by the
emulator's in-process harness (`edifier_link::Session` over an in-memory transport). Verifying the
over-the-air client path therefore needs the real headset or a second host in radio range.

The emulator's own documentation (`crates/edifier-link/examples/emulate.rs`) lists what it
implements and what it cannot reproduce; [macos-integration-limits.md](macos-integration-limits.md)
covers the surrounding platform evidence.
