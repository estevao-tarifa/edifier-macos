# Edifier for macOS

macOS cannot drive a third-party headset's noise control: Control Center only offers its
noise-control tile for Apple/Beats devices because that tile is fed by Apple's proprietary
accessory protocol, which Edifier firmware does not implement (evidence in
[docs/macos-integration-limits.md](docs/macos-integration-limits.md)). This project talks the Edifier
BLE control protocol directly instead: a Rust protocol and transport library, the `edifierctl` CLI,
an MCP server for coding agents, and an AppKit menu bar app that behaves like the AirPods
noise-control panel. The wire format was reverse engineered from the Edifier Connect captures
collected by [mEDIFIER](https://github.com/wh201906/mEDIFIER); see
[docs/protocol.md](docs/protocol.md) and [Credits](#credits).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.sh | sh
```

The installer builds the workspace when it runs from a checkout and otherwise downloads the release
asset for this architecture (verifying its `sha256` when one is published), installs `edifierctl`,
`edifier-mcp` and `edifier-menubar` into `~/.local/bin`, and offers to start the menu bar app at
every login. Useful flags: `--prefix DIR`, `--version vX.Y.Z`, `--from-source`,
`--no-launch-agent`, `--uninstall` (`--help` lists them all).

<details>
<summary>Publishing your own copy (fork, then tag a release)</summary>

1. Push this repository to GitHub and keep the branch named `main`.
2. Replace `<owner>/<repo>` above with your repository.
3. Tag a release: `git tag v0.1.0 && git push origin v0.1.0`. The `release` workflow builds
   `edifier-macos-aarch64.tar.gz` and `edifier-macos-x86_64.tar.gz` with checksums and attaches them
   to the GitHub release, which is what the installer downloads.
4. Users who clone instead of curling never need a release: the installer builds from source
   whenever it finds a checkout next to itself.
</details>

## Support matrix

Verified against EDIFIER W820NB over BLE on macOS 15.7.2 (Apple Silicon).

| Surface | Status on macOS 15 | CLI | MCP tool | Menu bar |
| --- | --- | --- | --- | --- |
| ANC off / noise cancelling / transparency | works | `anc off\|on\|ambient` | `edifier_anc` | ANC radio group |
| Transparency volume, -3..=3 | works | `ambient-volume <level>` | `edifier_ambient_volume` | volume submenu |
| EQ preset (normal/pop/classical/rock) | works | `eq <preset>` | `edifier_eq` | sound-effect submenu |
| Low-latency game mode | works | `game-mode on\|off` | `edifier_game_mode` | toggle |
| Voice-prompt volume 0..=15 | works | `prompt-volume <level>` | `edifier_prompt_volume` | volume submenu |
| Which gestures cycle which ANC mode | works | `gestures` | `edifier_gestures` | gesture submenu |
| Rename the headset | works | `name <new-name>` | `edifier_rename` | rename dialog |
| Battery, firmware, MAC, name | works | `battery`, `firmware`, `mac`, `status` | `edifier_status` | header + title |
| Auto-shutdown timer (off, 0..=180 min) | works | `shutdown-timer off\|<min>` | `edifier_shutdown_timer` | timer submenu |
| Move the classic audio link to this Mac | works | `connect-audio` | `edifier_connect_audio` | menu item |
| Forward playback buttons to the host | works | `playback <action>` | `edifier_playback` | — |
| Power off / re-pair / factory reset | works | `power-off`, `re-pair`, `factory-reset` (all `--yes`) | `edifier_destructive` | confirmed items |
| Agent control from scripts and agents | works | any command with `--json` | the whole `edifier_*` set | — |
| LDAC 48 kHz / 96 kHz | no macOS LDAC encoder: audio stops until the mode is set back to `off`; `--force` / `confirm=true` required | `ldac 48k\|96k` | `edifier_ldac` | LDAC submenu |
| Apple's Control Center noise-control tile | fed by Apple's proprietary accessory protocol, not a public API | — | — | — |
| kext / DriverKit codec or ANC takeover | no Bluetooth-audio DriverKit family; a CoreAudio HAL plug-in cannot select the A2DP codec or reach the ANC | — | — | — |

The auto-power-off flag, the device fingerprint and the raw playback state are reachable only through `raw` / `edifier_raw`.

### Verified against the real headset

`scripts/hardware-functions.sh` runs the whole matrix against the device (capturing every value first
and restoring it afterwards; nothing destructive is sent). On an EDIFIER W820NB running firmware
`01.00.02` over the macOS-held BLE link, the observed result was:

| Area | Observed |
| --- | --- |
| Battery / firmware / MAC / name | `34%`, `01.00.02`, `0C:AE:BD:BC:8F:A2`, `EDIFIER W820NB` |
| ANC off / on / transparency | all three switch and report back |
| Ambient volume, game mode, prompt volume, shutdown timer | applied and reported back (timer as `30 min`, then cleared) |
| Rename | acknowledged (`ack cmd=0xCA`), reads back unchanged when written with the same name |
| Playback | answers with a transport state (`0x0D` for `play`) |
| Sound effect (`C4`/`D5`), ANC gestures (`F0`/`F1`), LDAC (`48`/`49`) | **silent — not implemented on this unit** (matches mEDIFIER hiding those groups for the W820NB, except LDAC, which the original W820NB also lacks) |

Unimplemented commands fail with a timeout instead of pretending to succeed, which is why
`--timeout-ms` exists. The menu bar app gates entries by model, so a W820NB shows the sound-effect and
gesture submenus as "Not supported by this headset".

## Install and build

```sh
cargo build --release
```

No system-wide changes are made: everything runs as the logged-in user (an optional per-user
LaunchAgent starts the menu bar app at login).

The first CoreBluetooth call triggers macOS's one-time Bluetooth permission prompt for the terminal
(or the app bundle); grant it in **System Settings → Privacy & Security → Bluetooth**. CoreBluetooth
blocks until the prompt is answered, so a client that gets no answer gives up after 20 s. No root
and no SIP change are needed.

## CLI

```sh
cargo run -p edifierctl -- scan --scan-seconds 8   # list BLE control endpoints
cargo run -p edifierctl -- status                  # battery, ANC, EQ, LDAC, timers, …
cargo run -p edifierctl -- --device "W820NB" anc on
cargo run -p edifierctl -- doctor                  # scan + connect + query everything
cargo run -p edifierctl -- --json status           # machine-readable Reply
```

`--device` takes a name fragment or a 36-character CoreBluetooth identifier, falling back to
`EDIFIER_DEVICE` and then to the first Edifier endpoint found. Destructive commands and non-off LDAC
modes refuse to run without `--yes` / `--force`; `monitor` streams device notifications;
`--timeout-ms` bounds how long one command may take to answer (default 3000).

### How the headset is found

macOS keeps a BLE link open to a paired headset (it shows up as `EDIFIER BLE` in System Settings and
in `system_profiler SPBluetoothDataType`), and a *connected* peripheral stops advertising — so a scan
alone cannot see the normal case. Connecting therefore asks CoreBluetooth for the peripherals the
system already has connected (`-[CBCentralManager retrieveConnectedPeripheralsWithServices:]`) and
only falls back to scanning when that returns nothing. Both paths drive the same session code; the
menu bar app and the MCP server use the same order.

## Menu bar app

```sh
cargo run -p edifier-menubar                               # run the app
cargo run -p edifier-menubar -- --install-launch-agent     # writes ~/Library/LaunchAgents/sh.edifier.menubar.plist
cargo run -p edifier-menubar -- --uninstall-launch-agent
```

It scans for the remembered CoreBluetooth identifier (persisted in `~/Library/Application Support/edifier-menubar/config.json`), keeps a session open with reconnect backoff, shows battery in the menu bar title, and rebuilds the menu with checkmarks on every state change. The install flag prints the `launchctl bootstrap gui/$UID <plist>` command to run.

## MCP server

`edifier-mcp` speaks newline-delimited JSON-RPC 2.0 over stdio, keeps one BLE control link open,
and sends diagnostics to stderr only. Register it in `~/.omp/agent/mcp.json`:

```json
{"mcpServers": {"edifier": {
  "type": "stdio",
  "command": "/ABSOLUTE/PATH/macnativeedifire/target/release/edifier-mcp",
  "timeout": 60000
}}}
```

`edifier-mcp [--device <name-or-uuid>]`; the selector also comes from `EDIFIER_DEVICE`. Tools:
`edifier_scan`, `edifier_connect`, `edifier_status`, `edifier_anc`, `edifier_ambient_volume`,
`edifier_eq`, `edifier_game_mode`, `edifier_ldac`, `edifier_prompt_volume`, `edifier_gestures`,
`edifier_shutdown_timer`, `edifier_rename`, `edifier_playback`, `edifier_raw`,
`edifier_connect_audio`, `edifier_disconnect` (closes the control link only), `edifier_destructive`.

## Layout

| Crate | Responsibility |
| --- | --- |
| `edifier-proto` | Framing and checksum, the command table, device-event decoding, the status snapshot, and the model/feature matrix. No I/O. |
| `edifier-link` | BLE GATT transport (btleplug) and the request/response `Session` that correlates commands with device notifications. |
| `edifierctl` | Command line front end (`edifierctl`). |
| `edifier-macos` | IOBluetooth helpers: list paired classic devices, move the audio link back to this Mac. |
| `edifier-mcp` | MCP stdio server exposing the `edifier_*` tool set. |
| `edifier-menubar` | AppKit menu bar app: status item, AirPods-style control panel, LaunchAgent install. |

Supporting files:

| Path | Purpose |
| --- | --- |
| `scripts/install.sh` | The installer behind the `curl \| sh` one-liner (`--prefix`, `--version`, `--from-source`, `--launch-agent`, `--uninstall`). |
| `scripts/verify.sh` | Build → tests → MCP handshake → emulator round trip → real headset when present. |
| `scripts/hardware-functions.sh` | Exercises every function against the real headset and restores the previous values. |
| `docs/protocol.md` | The reverse-engineered wire format, command table and quirks. |
| `docs/macos-notes.md`, `docs/macos-integration-limits.md` | Platform behaviour and the measured evidence for what macOS cannot do. |
| `.github/workflows/` | `ci.yml` (fmt, clippy, tests, end-to-end checks) and `release.yml` (tag → tarballs + checksums). |

## Protocol

Framing is `AA|len|payload|sum_hi|sum_lo` out and `BB`/`CC` in, with an additive checksum seeded
with `0x2019`; each payload starts with a one-byte command code. The command table, BLE UUIDs and
chunking, model differences, quirks and the test workflow are in [docs/protocol.md](docs/protocol.md).

## Credits

- Reverse engineered from the Edifier Connect traffic captured by [mEDIFIER](https://github.com/wh201906/mEDIFIER) (`Qt/comms/comm.cpp` framing, `Qt/devices/basedevice.cpp` commands,
  `doc/KnownCommands/W820NB_Double_Gold.txt` captures, `Qt/devices/deviceinfo.json` feature matrix);
  every captured frame is re-verified byte for byte in `crates/edifier-proto/tests/vectors.rs`.
- No vendor support: the firmware is undocumented, behaviour can change between firmware versions,
  and anything not reproduced in the capture vectors is best effort.
- Destructive commands (power off, re-pair, factory reset, non-off LDAC) interrupt the link and
  require an explicit confirmation flag.
