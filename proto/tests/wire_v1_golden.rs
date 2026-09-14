//! Golden-Test für das Wire-Format v1.
//!
//! Jede Zeile in `tests/fixtures/wire_v1.jsonl` deckt eine Variante von
//! [`logsentry_proto::ClientMessage`] oder [`logsentry_proto::ServerMessage`]
//! ab. Der Test liest jede Zeile, deserialisiert sie, serialisiert sie neu
//! und vergleicht das Ergebnis byte-genau mit der Originalzeile. Das
//! garantiert: Feldnamen, `rename_all`, Tag-Namen und Varianten-Reihenfolge
//! bleiben stabil, solange dieser Test grün ist. Bricht der Test durch eine
//! absichtliche Protokolländerung, muss `PROTOCOL_VERSION` erhöht und diese
//! Fixture bewusst neu erzeugt werden.

use logsentry_proto::{ClientMessage, ServerMessage};

const FIXTURE: &str = include_str!("fixtures/wire_v1.jsonl");

/// Nachrichten aus Client- und Server-Richtung, nur um sie mit einem
/// gemeinsamen Deserializer anhand des `type`-Tags zu unterscheiden. Kein
/// Teil des Wire-Formats selbst — beide Enums bleiben getrennt, damit ein
/// Daemon niemals versehentlich eine `ClientMessage` sendet oder umgekehrt.
fn round_trip(raw: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(raw).expect("Fixture-Zeile muss gültiges JSON sein");
    let tag = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .expect("Fixture-Zeile muss ein \"type\"-Feld haben");

    const CLIENT_TAGS: &[&str] = &["hello", "subscribe", "get_context", "action", "ping"];
    const SERVER_ONLY_TAGS: &[&str] = &[
        "recent_anomalies",
        "snapshot",
        "anomaly",
        "context",
        "action_result",
        "pong",
        "error",
        "goodbye",
    ];

    if SERVER_ONLY_TAGS.contains(&tag) {
        let msg: ServerMessage =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("ServerMessage {tag:?}: {e}"));
        serde_json::to_string(&msg).expect("Reserialisierung darf nicht fehlschlagen")
    } else if CLIENT_TAGS.contains(&tag) && tag == "hello" {
        // "hello" existiert in beiden Richtungen mit unterschiedlichen
        // Feldern (Client sendet client_name/client_version, Server sendet
        // daemon_version/hostname/session_id/...). Anhand der Feldmenge
        // unterscheiden.
        if value.get("client_name").is_some() {
            let msg: ClientMessage =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("ClientMessage {tag:?}: {e}"));
            serde_json::to_string(&msg).expect("Reserialisierung darf nicht fehlschlagen")
        } else {
            let msg: ServerMessage =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("ServerMessage {tag:?}: {e}"));
            serde_json::to_string(&msg).expect("Reserialisierung darf nicht fehlschlagen")
        }
    } else if CLIENT_TAGS.contains(&tag) {
        let msg: ClientMessage =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("ClientMessage {tag:?}: {e}"));
        serde_json::to_string(&msg).expect("Reserialisierung darf nicht fehlschlagen")
    } else {
        panic!("unbekannter Tag {tag:?} in Fixture-Zeile: {raw}");
    }
}

#[test]
fn jede_fixture_zeile_ist_stabil_unter_deserialisierung_und_reserialisierung() {
    let mut checked = 0usize;
    for (line_no, raw) in FIXTURE.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let reserialized = round_trip(raw);
        assert_eq!(
            reserialized,
            raw,
            "Zeile {} weicht nach Rundweg ab:\n  original: {}\n  neu:      {}",
            line_no + 1,
            raw,
            reserialized
        );
        checked += 1;
    }
    assert!(
        checked >= 20,
        "Fixture sollte alle Wire-Varianten abdecken, nur {checked} Zeilen geprüft"
    );
}

#[test]
fn unit_name_lehnt_unzulaessige_zeichen_und_suffixe_ab() {
    use logsentry_proto::UnitName;

    assert!(UnitName::parse("sshd.service").is_ok());
    assert!(UnitName::parse("getty@tty1.service").is_ok());
    assert!(UnitName::parse("").is_err());
    assert!(UnitName::parse("sshd").is_err(), "fehlender Suffix");
    assert!(UnitName::parse("sshd.exe").is_err(), "unbekannter Suffix");
    assert!(
        UnitName::parse("rm -rf /.service").is_err(),
        "Leerzeichen und Shell-Metazeichen müssen abgelehnt werden"
    );
    assert!(
        UnitName::parse(&format!("{}.service", "a".repeat(300))).is_err(),
        "über 255 Zeichen"
    );
}

#[test]
fn action_request_serialisiert_unit_name_als_reinen_string() {
    use logsentry_proto::{ActionRequest, UnitName};

    let req = ActionRequest::RestartUnit {
        unit: UnitName::parse("sshd.service").unwrap(),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert_eq!(json, r#"{"kind":"restart_unit","unit":"sshd.service"}"#);
}
