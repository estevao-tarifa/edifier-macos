//! Ground-truth vectors captured from the Edifier Connect app traffic (as recorded in
//! mEDIFIER's `doc/KnownCommands/W820NB_Double_Gold.txt`) plus frames reconstructed from the
//! same protocol rules. Every `sendData:` line in that capture is asserted byte for byte, so a
//! regression in framing or the command table fails here instead of on real hardware.

use edifier_proto::command::AmbientVolume;
use edifier_proto::event::ShutdownTimer;
use edifier_proto::{
    AncMode, Command, ControlSettings, Event, Frame, FrameDecoder, Head, LdacMode, SoundEffect,
    Status,
};

fn hex(text: &str) -> Vec<u8> {
    let clean: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    assert_eq!(clean.len() % 2, 0, "odd hex length in {text:?}");
    (0..clean.len() / 2)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// Build a device -> host frame with a correct checksum.
fn device_frame(head: Head, payload: &[u8]) -> Vec<u8> {
    Frame {
        head,
        payload: payload.to_vec(),
    }
    .encode()
    .unwrap()
}

#[test]
fn host_frames_match_the_captured_traffic() {
    let vectors: &[(Command, &str)] = &[
        (Command::SetAnc(AncMode::Normal), "AA02C1012187"),
        (Command::SetAnc(AncMode::NoiseCancelling), "AA02C1022188"),
        (Command::SetAnc(AncMode::Ambient), "AA02C1032189"),
        (
            Command::SetAmbientVolume(AmbientVolume(3)),
            "AA03C103092193",
        ),
        (
            Command::SetAmbientVolume(AmbientVolume(2)),
            "AA03C103082192",
        ),
        (
            Command::SetAmbientVolume(AmbientVolume(0)),
            "AA03C103062190",
        ),
        (
            Command::SetAmbientVolume(AmbientVolume(-3)),
            "AA03C10303218D",
        ),
        (
            Command::SetControlSettings(ControlSettings::all()),
            "AA03F10A0721C8",
        ),
        (
            Command::SetControlSettings(ControlSettings {
                normal: false,
                noise_reduction: true,
                ambient: true,
            }),
            "AA03F10A0621C7",
        ),
        (Command::SetPromptVolume(0x07), "AA02060720D2"),
        (Command::SetPromptVolume(0x0F), "AA02060F20DA"),
        (Command::PowerOff, "AA01CE2192"),
        (Command::Disconnect, "AA01CD2191"),
        (Command::RePair, "AA01CF2193"),
        (Command::FactoryReset, "AA010720CB"),
        (Command::SetShutdownTimer(Some(5)), "AA03D10005219C"),
        (Command::SetShutdownTimer(Some(15)), "AA03D1000F21A6"),
        (Command::SetShutdownTimer(Some(30)), "AA03D1001E21B5"),
        (Command::SetShutdownTimer(Some(60)), "AA03D1003C21D3"),
        (Command::SetShutdownTimer(Some(180)), "AA03D100B4224B"),
        (Command::SetShutdownTimer(None), "AA01D22196"),
        (Command::SetGameMode(true), "AA02090120CF"),
        (Command::SetGameMode(false), "AA02090020CE"),
        (Command::SetSoundEffect(SoundEffect::Rock), "AA02C403218C"),
        (
            Command::SetSoundEffect(SoundEffect::Classical),
            "AA02C402218B",
        ),
        (Command::SetName("abc".into()), "AA04CA61626322B7"),
        (Command::SetName("12345".into()), "AA06CA31323334352292"),
        (
            Command::SetName("中文测试".into()),
            "AA0DCAE4B8ADE69687E6B58BE8AF952A38",
        ),
        (Command::SetLdac(LdacMode::Off), "AA024900210E"),
        (Command::SetLdac(LdacMode::Khz48), "AA024901210F"),
        (Command::SetLdac(LdacMode::Khz96), "AA0249022110"),
        (Command::QueryFingerprint, "AA01D8219C"),
        (Command::QueryName, "AA01C9218D"),
        (Command::QueryPlayback, "AA01C32187"),
        (Command::QueryBattery, "AA01D02194"),
        (Command::QueryMac, "AA01C8218C"),
        (Command::QueryFirmware, "AA01C6218A"),
        (Command::QueryAnc, "AA01CC2190"),
        (Command::QueryControlSettings, "AA02F00A21BF"),
        (Command::QueryLdac, "AA0148210C"),
        (Command::QueryPromptVolume, "AA010520C9"),
        (Command::QueryGameMode, "AA010820CC"),
        (Command::QueryShutdownTimer, "AA01D32197"),
        (Command::QuerySoundEffect, "AA01D52199"),
    ];

    for (command, expected) in vectors {
        let encoded = command.encode().unwrap();
        assert_eq!(
            encoded,
            hex(expected),
            "{} encoded as {:02X?}",
            command.label(),
            encoded
        );
    }
}

#[test]
fn device_frames_decode_into_typed_events() {
    // Every `readData:` line that carries a value, decoded from the capture.
    let vectors: &[(&str, Event)] = &[
        (
            "BB03C10106219F",
            Event::Anc {
                mode: AncMode::Normal,
                ambient_volume: AmbientVolume(0),
            },
        ),
        (
            "BB03C1020621A0",
            Event::Anc {
                mode: AncMode::NoiseCancelling,
                ambient_volume: AmbientVolume(0),
            },
        ),
        (
            "BB03C1030921A4",
            Event::Anc {
                mode: AncMode::Ambient,
                ambient_volume: AmbientVolume(3),
            },
        ),
        (
            "BB03CC010621AA",
            Event::Anc {
                mode: AncMode::Normal,
                ambient_volume: AmbientVolume(0),
            },
        ),
        ("BB02D04D21F3", Event::Battery(77)),
        ("BB02D50021AB", Event::SoundEffect(SoundEffect::Normal)),
        ("BB02080020DE", Event::GameMode(false)),
        ("BB024800211E", Event::Ldac(LdacMode::Off)),
        ("BB024801211F", Event::Ldac(LdacMode::Khz48)),
        ("BB02050720E2", Event::PromptVolume(7)),
        (
            "BB03F00A0721D8",
            Event::ControlSettings(ControlSettings::all()),
        ),
        (
            "BB02D30021A9",
            Event::ShutdownTimer(ShutdownTimer::Disabled),
        ),
        (
            "BB03D3000521AF",
            Event::ShutdownTimer(ShutdownTimer::Minutes(5)),
        ),
        (
            "CC02CA0121B2",
            Event::Ack {
                command: 0xCA,
                status: vec![0x01],
            },
        ),
        (
            "CC02D20121BA",
            Event::Ack {
                command: 0xD2,
                status: vec![0x01],
            },
        ),
    ];

    for (raw, expected) in vectors {
        let frame = Frame::parse(&hex(raw)).unwrap_or_else(|err| panic!("{raw}: {err}"));
        assert_eq!(&Event::parse(&frame), expected, "frame {raw}");
    }
}

#[test]
fn variable_length_responses_decode() {
    let mac = device_frame(Head::State, &hex("C80CAEBDBC8FA2"));
    let frame = Frame::parse(&mac).unwrap();
    assert_eq!(
        Event::parse(&frame),
        Event::Mac([0x0C, 0xAE, 0xBD, 0xBC, 0x8F, 0xA2])
    );

    let firmware = device_frame(Head::State, &hex("C6030002"));
    let frame = Frame::parse(&firmware).unwrap();
    assert_eq!(Event::parse(&frame), Event::Firmware("03.00.02".into()));

    let name = device_frame(Head::State, &[0xC9, b'W', b'8', b'2', b'0', b'N', b'B']);
    let frame = Frame::parse(&name).unwrap();
    assert_eq!(Event::parse(&frame), Event::Name("W820NB".into()));

    let fingerprint = device_frame(Head::State, &hex("D8AABBCCDDEEFF00112233445566778899AABB"));
    let frame = Frame::parse(&fingerprint).unwrap();
    assert_eq!(
        Event::parse(&frame),
        Event::Fingerprint("AABBCCDDEEFF00112233445566778899AABB".into())
    );
}

#[test]
fn status_accumulates_and_summarises() {
    let mut status = Status::new();
    for raw in [
        "BB02D04D21F3",
        "BB03C1020621A0",
        "BB024801211F",
        "BB02090120E0",
        "BB04C603000221A3",
    ] {
        let frame = Frame::parse(&hex(raw)).unwrap();
        Event::parse(&frame).apply(&mut status);
    }

    assert_eq!(status.battery, Some(77));
    assert_eq!(status.anc, Some(AncMode::NoiseCancelling));
    assert_eq!(status.ldac, Some(LdacMode::Khz48));
    assert_eq!(status.game_mode, Some(true));
    assert_eq!(status.firmware.as_deref(), Some("03.00.02"));
    assert_eq!(status.summary(), "77% · ANC on · LDAC 48k · game mode");

    let json = serde_json::to_string(&status).unwrap();
    assert!(json.contains("\"battery\":77"), "{json}");
    assert!(json.contains("\"anc\":\"noise_cancelling\""), "{json}");
}

#[test]
fn decoder_reassembles_split_frames_and_resynchronises() {
    let mut decoder = FrameDecoder::new();
    let mut stream = vec![0x00, 0xFF]; // line noise before the first frame
    stream.extend(device_frame(Head::State, &hex("D04D")));
    stream.extend(device_frame(Head::State, &hex("C10106")));

    decoder.push(&stream[..5]);
    assert!(decoder.next_frame().is_none());

    decoder.push(&stream[5..]);
    let first = decoder.next_frame().unwrap().unwrap();
    assert_eq!(Event::parse(&first), Event::Battery(77));

    let second = decoder.next_frame().unwrap().unwrap();
    assert_eq!(
        Event::parse(&second),
        Event::Anc {
            mode: AncMode::Normal,
            ambient_volume: AmbientVolume(0),
        }
    );
    assert!(decoder.next_frame().is_none());
}

#[test]
fn corrupted_checksum_is_rejected() {
    let mut raw = device_frame(Head::State, &hex("D04D"));
    let last = raw.len() - 1;
    raw[last] ^= 0xFF;
    assert!(Frame::parse(&raw).is_err());
}

#[test]
fn decoder_resynchronises_after_a_corrupted_frame() {
    let mut corrupted = device_frame(Head::State, &hex("D04D"));
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xFF;

    let mut good = device_frame(Head::State, &hex("C10206"));
    let mut stream = corrupted;
    stream.append(&mut good);

    let mut decoder = FrameDecoder::new();
    decoder.push(&stream);
    // The first pass reports the bad checksum (the caller logs and continues).
    assert!(decoder.next_frame().unwrap().is_err());
    // The valid frame that follows must still be recovered.
    let frame = decoder.next_frame().unwrap().unwrap();
    assert_eq!(
        Event::parse(&frame),
        Event::Anc {
            mode: AncMode::NoiseCancelling,
            ambient_volume: AmbientVolume(0),
        }
    );
}

#[test]
fn ambient_volume_round_trips_through_the_wire_encoding() {
    for volume in -3..=3i8 {
        let encoded = AmbientVolume(volume);
        assert_eq!(AmbientVolume::from_wire(encoded.wire()).0, volume);
    }
    assert!(AmbientVolume::new(4).is_err());
    assert!(AmbientVolume::new(-4).is_err());
}
