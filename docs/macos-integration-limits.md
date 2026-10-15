# macOS integration limits — a third-party ANC headset on macOS 15

Host: macOS 15.7.2 (24G325), Apple M4. Controller `BCM_4388C2`, PCIe, firmware 22.5.190.764,
`HCIT PCIe, LMPv 13 (5.4)`. `csrutil status` → `enabled`; `Authenticated Root status: enabled`;
`mount` → `/ (apfs, sealed, local, read-only)`. Every claim is a command run on this machine;
anything not directly observed is marked `[INFERENCE]`. Framework binaries are cache-only on
macOS 15, so their strings were read from the arm64e dyld shared cache (`dyld_shared_cache_arm64e*`:
image ranges from `…arm64e.map`, vmaddr→file offset via the cache header mapping table; `/tmp/dsc.py`).

## Q1 — macOS 15 ships no LDAC (and no aptX) A2DP encoder

Substring counts, `strings -a -n 4 <bin> | grep -c -i <tok>`:

| binary | ldac | aptx | sbc | aac | lc3 |
|---|---|---|---|---|---|
| `/usr/sbin/bluetoothd` | 2\* | 0 | 49 | 286 | 3 |
| `…/Audio/Plug-Ins/HAL/BTAudioHALPlugin.driver/Contents/MacOS/BTAudioHALPlugin` | 0 | 0 | 44 | 31 | 19 |
| `/usr/libexec/bluetoothuserd` | 1\* | 0 | 0 | 0 | 0 |
| `/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter` | 0 | 0 | 0 | 1 | 0 |

\* case-insensitive substrings inside unrelated words, not codec support: `/usr/sbin/bluetoothd` →
`HIDShimBuildACLPacket handle without session`, `holdActiveCalls:`; `/usr/libexec/bluetoothuserd` →
`listener:shouldAcceptNewConnection:`. The whole arm64e system cache has no LDAC either
(`LC_ALL=C grep -a -c -o LDAC <cachefile>`): `dyld_shared_cache_arm64e` → `0`, `…arm64e.01` → `3`, all
byte noise (`…SCHILDACvpZMV…`, `…V6EAGAINACvpZMV…`, `…ygXLDACHPX...`); aptX → `0` in both.
The encoders that exist are SBC / AAC-LC / AAC-ELD / LC3:
```
$ strings -a -n 6 …/BTAudioHALPlugin | grep -E 'Creating .* Encoder|EncoderFactory'
Creating AAC-ELD Encoder / Creating AAC-LC Encoder / Creating SBC Encoder / ACLC3EncoderFactory
$ strings -a -n 6 /usr/sbin/bluetoothd | grep -E 'AVDTP_CODEC|A2DP configured'
AVDTP_CODEC_SBC / AVDTP_CODEC_MPEG_AAC / AVDTP_CODEC_APPLE_AACELD
A2DP configured at %2.1f KHz. Codec: SBC, Bitpool: %d (%d kbps)… / … Codec: AAC-LC, VBR max: %d kbps…
```
`afconvert -hf 2>&1 | grep -ciE 'sbc|ldac'` → `0`: not even a CoreAudio data format. Controller side
(`system_profiler SPBluetoothDataType`): `Supported services: 0x392039 < HFP AVRCP A2DP HID Braille
LEA AACP GATT SerialPort >` — a service mask, no codec list; the A2DP codec set is the host's, above.

**Conclusion.** A headset set to "LDAC 48k/96k" (`49 01` / `49 02`) cannot receive LDAC from this Mac,
and no codec can be added (Q3). If the headset still advertises SBC/AAC SEIDs in LDAC mode, AVDTP
selects SBC/AAC and audio keeps working. If it advertises only the LDAC vendor SEID there is no common
codec: A2DP setup cannot complete, so there is no media route — the device stays paired/connected and
HFP (mSBC/CVSD) remains for calls only. `[INFERENCE]`: derived from the encoder inventory plus AVDTP
semantics; the headset was `Not Connected` during collection, so no live negotiation was captured. The
project records the same expectation (`crates/edifier-proto/src/command.rs`: "Selecting a non-`Off`
mode makes the headset expect an LDAC source, so a source without an LDAC encoder (every macOS release
to date) loses audio.").

## Q2 — Noise Control exists only for Apple/Beats because it is fed by AACP

Per-device host state, unified log, `bluetoothd` device monitor (`SupS` = SDP-derived services):
```
AirPods Pro   41:42:7E:A9:DF:49   VID 0x004C
  SupS 0x82019  < HFP AVRCP A2DP Braille AACP >
  DvF 0x5000D014037 < AACP MagicPaired ShareAudio Tipi PSE ClassicPaired HIDGoodBehavior AdvancedAppleAudio ANC Transparency SmartRouting Connectable >
EDIFIER W820NB 0C:AE:BD:BC:8F:A2   SupS 0x202019 < HFP AVRCP A2DP Braille SerialPort >
  DvF 0x40000014000 < ClassicPaired HIDGoodBehavior Connectable >
  bluetoothd[…][Server.AACP] getInEarStatus: No AACP connection for 0C:AE:BD:BC:8F:A2
```
The EDIFIER advertises no AACP service and none of the `AdvancedAppleAudio ANC Transparency
SmartRouting` flags, and bluetoothd's AACP client has no connection to it — there is no in-ear or
listening-mode state for a Noise Control panel to show. Live SDP enumeration could not be run: both
devices are `Not Connected` (`system_profiler SPBluetoothDataType`), and
`IOBluetoothDevice performSDPQuery:` needs a reachable device plus Bluetooth TCC consent — `SupS` above
is the cached SDP-derived service mask instead.

The UI is fed by Apple-private API:
```
/usr/sbin/bluetoothd: AACPClient::adaptiveVolumeMessageRecvEvent…, AACP_MSG_*, allowOffListeningMode,
  "AudioDevice. SupportsInEarDetection %d, isAppleAudioDevice %d, InEarDetectionEnabled %d SpatialSupport
  %s … supportedListeningModes %u", ANC/Transparency/Auto, Beats/Beats Studio…
/System/Library/PrivateFrameworks/CoreBluetoothUI.framework/Versions/A/CoreBluetoothUI:
  AudioOptionsViewNoiseControl, setListeningModeConfigsForNormal:anc:awareness:, toggleANCListeningMode:,
  isAppleAccessoryServer, isAACPCapabilityBit:
/System/Library/PrivateFrameworks/BluetoothManager.framework/Versions/A/BluetoothManager:
  BTAccessoryManagerGetAACPCapabilityInteger, getAACPCapabilityBit:, isGenuineAirPods,
  "received 'AACP In Ear Status Changed' event from device %@"
/System/Library/PrivateFrameworks/HeadphoneManager.framework/Versions/A/HeadphoneManager:
  BTAirPodsControlServiceClient, ListeningModeV2, NoiseControl, pressHoldTransparency
/System/Library/Frameworks/IOBluetooth.framework/Versions/A/IOBluetooth:
  _AACPConnectionUUID, sendAACPCustomData:withData:forDevice:, setListeningMode:
/System/Library/CoreServices/ControlCenter.app/Contents/MacOS/ControlCenter: ListeningModeDevice,
  availableBluetoothListeningModes, currentBluetoothListeningMode, listeningModeOffAllowed,
  AVOutputDeviceBluetoothListeningMode, com.apple.airpods-max-2024
```
`AVOutputDevice` exists at runtime in AVFoundation (dlopen + `class_copyMethodList` probe:
`-availableBluetoothListeningModes`, `-currentBluetoothListeningMode`,
`-setCurrentBluetoothListeningMode:error:`, `-isAppleAccessory`, `-batteryLevel`), but no macOS SDK
header on this machine declares it, and `-isAppleAccessory` is the gate: listening-mode values come over
AACP (`[INFERENCE]` — the EDIFIER side of the same channel is provably absent, see the log line above),
which a non-Apple accessory does not have.

Why a third party cannot inject that UI:
```
$ csrutil status -> enabled; Authenticated Root: enabled; mount | grep ' / ' -> / (apfs, sealed, local, read-only)
$ codesign -dvvv …/ControlCenter.app -> Platform identifier=16, Authority=Software Signing → Apple Root CA
$ codesign -d --entitlements :- … -> 190 entitlements, incl. com.apple.bluetooth.system,
  com.apple.bluetooth.internal, com.apple.avfoundation.allows-set-output-device
$ DYLD_INSERT_LIBRARIES=/tmp/libinject.dylib /usr/bin/true -> constructor never ran; DYLD_PRINT_LIBRARIES=1
  /usr/bin/true -> no output (DYLD_* stripped); same dylib into a locally built binary -> "INJECTED-OK"
```
Control Center is an Apple platform binary holding private Bluetooth/AirPlay entitlements on a sealed
read-only root, and DYLD injection into platform binaries is refused. Reproducing its Noise Control
panel for this headset would need SIP and the authenticated root disabled (or an Apple-signed binary
with `com.apple.bluetooth.system`), per-OS-build patching of ControlCenter/`CoreBluetoothUI`, and the
private AACP/`AVOutputDevice` API — and it would still show nothing, there being no AACP channel.
Risk: unsupported, breaks on updates, permanently weakens the machine.

## Q3 — No kext or DriverKit extension can take over Bluetooth audio or ANC

```
$ kmutil showloaded | wc -l           -> 256 lines (header + com.apple.* only; grep -vi com.apple: none)
$ ls /Library/Extensions              -> EPSONUSBPrintClass.kext HighPointIOP.kext HighPointRR.kext (none loaded)
$ ls /Library/DriverExtensions        -> empty ; systemextensionsctl list -> 1: io.tailscale.ipn.macsys.network-extension
$ ls /System/Library/DriverExtensions -> 17 Apple dexts; only Bluetooth one is IOUserBluetoothSerialDriver.dext (serial)
$ ls SDK …/Frameworks/*DriverKit*     -> DriverKit.framework only (no Bluetooth family)
```
There is no Bluetooth HCI/A2DP/HFP DriverKit family and no kext path on Apple Silicon. Codec
negotiation and the AACP/ANC state machine live inside `bluetoothd` + `BTAudioHALPlugin`, reached over
entitlement-gated XPC — `BTAudioHALPlugin` Info.plist `AudioServerPlugIn_MachServices`:
`com.apple.BTAudioHALPlugin.xpc`, `com.apple.BTAudioHALPluginAccessories`,
`com.apple.BTLEAudioController.xpc`, `com.apple.BTServer`, `com.apple.BTServer.le`, … . Answer: a
"parallel driver" that publishes an audio endpoint *and* drives the headset's codec cannot exist here:
no third-party kext/DEXT is loaded, and no family that could host one exists.

What is allowed:
- **CoreAudio HAL plug-in** (`AudioServerPlugIn.h`; installed under `/Library/Audio/Plug-Ins/HAL`,
  root-owned, needs root + `coreaudiod` restart — that directory does not exist on this Mac, i.e. none
  installed). It can publish a virtual device and process audio, but cannot choose the A2DP codec
  (bluetoothd does: `Bad48KHzCodecs: Device is in 48 KHz AAC allowlist`) or touch the headset's ANC.
- **LaunchAgent / LaunchDaemon**: run code at login/boot as user or root, enough to host the BLE session
  or a menu bar app; no entitlement escalation, and Bluetooth access still passes through TCC.

## Q4 — What actually works

| surface | mechanism | limit |
|---|---|---|
| CoreBluetooth BLE GATT | `btleplug` in `crates/edifier-link` (service `4809xxxx-1a48-11e9-ab14-d663bd873d93`, notify `…01`, write `…02`) | one TCC prompt (`kTCCServiceBluetoothAlways`; on this Mac only `com.google.Chrome` is granted); no root, no SIP change; reaches only what the headset exposes over BLE |
| CoreAudio HAL plug-in | `AudioServerPlugIn` virtual device | no codec choice, no ANC (Q3) |
| HFP / IOBluetooth profile connect | `IOBluetoothDevice openConnection…`, `IOBluetoothHandsFreeDevice` (the `blueutil` approach); HFP battery reporting (`AT+XAPL`) surfaces in the system UI while connected | forces the audio link to this Mac; adds no codec, no ANC; `blueutil` is not installed here; battery seen only for connected HFP devices (`[INFERENCE]` — no device was connected) |
| Automation | `/usr/bin/shortcuts`, `/usr/bin/osascript`, App Intents from the menu bar app | only the app's own verbs; no Control Center surface |

**Achievable "AirPods-like"**: a Rust/AppKit menu bar panel mirroring the AirPods noise-control UI
(Off / ANC / Transparency plus the headset's own modes, read and written over BLE), with the same verbs
exposed to agents via MCP/CLI. It is a look-alike backed by the headset's BLE protocol
(`crates/edifier-proto`), not an integration: it cannot appear in Control Center, cannot set the A2DP
codec, and gets no AirPods-only behaviour (in-ear detection, spatial audio, share audio, system-UI
battery).

## Evidence limits
- Live SDP enumeration and live AVDTP negotiation not run: both devices `Not Connected` (SDP also needs
  TCC consent). Framework strings read from the dyld shared cache; on-disk
  `BluetoothUIService.app/Contents/MacOS/BluetoothUIService` is a stub (8-byte `__text`, no strings).
- Not attempted: any change to system settings, SIP, or TCC; no writes outside `docs/`.
