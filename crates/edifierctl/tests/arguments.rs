//! Argument parsing and rendering tests that need no headset.

use edifier_link::ble::Target;
use edifier_proto::event::ShutdownTimer;
use edifier_proto::{AncMode, ControlSettings, LdacMode, SoundEffect, Status};

use edifierctl::{parse_hex_byte, parse_payload, parse_timer, render_status};

#[test]
fn timer_arguments_parse() {
    assert_eq!(parse_timer("off").unwrap(), ShutdownTimer::Disabled);
    assert_eq!(parse_timer("OFF").unwrap(), ShutdownTimer::Disabled);
    assert_eq!(parse_timer("0").unwrap(), ShutdownTimer::Disabled);
    assert_eq!(parse_timer("5").unwrap(), ShutdownTimer::Minutes(5));
    assert_eq!(parse_timer("180").unwrap(), ShutdownTimer::Minutes(180));
    assert!(parse_timer("181").is_err());
    assert!(parse_timer("later").is_err());
}

#[test]
fn hex_payloads_parse_with_and_without_prefixes() {
    assert_eq!(parse_hex_byte("C1").unwrap(), 0xC1);
    assert_eq!(parse_hex_byte("0xc1").unwrap(), 0xC1);
    assert_eq!(parse_hex_byte("C").unwrap(), 0x0C);
    assert!(parse_hex_byte("CCC").is_err());
    assert!(parse_hex_byte("ZZ").is_err());
    assert_eq!(
        parse_payload(&["C1".into(), "02".into()]).unwrap(),
        vec![0xC1, 0x02]
    );
    assert!(parse_payload(&[]).unwrap().is_empty());
}

#[test]
fn device_selectors_distinguish_identifiers_from_names() {
    let identifier = "f0e1d2c3-b4a5-9687-7869-5a4b3c2d1e0f";
    assert!(matches!(
        Target::from_selector(identifier),
        Target::Identifier(_)
    ));
    assert!(matches!(
        Target::from_selector("EDIFIER W820NB"),
        Target::Name(_)
    ));
    // A short string with dashes must not be mistaken for an identifier.
    assert!(matches!(
        Target::from_selector("my-head-set"),
        Target::Name(_)
    ));
}

#[test]
fn status_rendering_lists_only_known_values() {
    let mut status = Status::new();
    assert!(render_status(&status).is_empty());

    status.battery = Some(62);
    status.anc = Some(AncMode::Ambient);
    status.sound_effect = Some(SoundEffect::Classical);
    status.ldac = Some(LdacMode::Off);
    status.anc_gestures = Some(ControlSettings::all());
    status.shutdown_timer = Some(ShutdownTimer::Minutes(30));
    status.name = Some("EDIFIER W820NB".into());
    status.model = Some(edifier_proto::DeviceModel::W820Nb);

    let rendered = render_status(&status);
    assert!(rendered.contains("battery          62%"), "{rendered}");
    assert!(rendered.contains("anc              ambient"), "{rendered}");
    assert!(
        rendered.contains("sound effect     classical"),
        "{rendered}"
    );
    assert!(rendered.contains("ldac             off"), "{rendered}");
    assert!(rendered.contains("anc gestures     0b111"), "{rendered}");
    assert!(rendered.contains("shutdown timer   30 min"), "{rendered}");
    assert!(rendered.contains("EDIFIER W820NB"), "{rendered}");
    assert!(!rendered.contains("mac"), "{rendered}");
}
