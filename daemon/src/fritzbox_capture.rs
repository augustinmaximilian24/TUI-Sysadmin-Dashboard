//! Fritz!Box-Paketmitschnitt (Phase-12-Erweiterung, optional): meldet sich
//! über die eingebaute Diagnose-Funktion der Box an (`login_sid.lua`,
//! MD5-Challenge-Response) und liest den dauerhaft offenen
//! Paketmitschnitt-Stream (`/cgi-bin/capture_notimeout`), um zu sehen,
//! wohin die per [`crate::lan_devices`] erkannten Geräte sich verbinden.
//! Nur Header-Felder (Ziel-IP/Port, Protokoll) -- keine Nutzdaten, kein
//! DPI.
//!
//! # ponytail
//! Nur der MD5-Login-Pfad ist abgedeckt (Fritz!OS-Versionen, die eine
//! einfache Challenge ausgeben). PBKDF2 (Fritz!OS 7.25+, Challenge beginnt
//! mit `"2$"`) wird erkannt und klar gemeldet statt falsch berechnet --
//! Nachziehen (zusätzlich `pbkdf2`/`hmac`/`sha2`), falls die eigene Box das
//! erzwingt.
//!
//! Nur IPv4 wird ausgewertet, dieselbe bewusste Grenze wie beim
//! bestehenden GeoIP-Modul (`gui/src/network_map/geoip.rs`).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use logsentry_core::config::FritzboxConfig;
use logsentry_proto::{LanFlowEvent, LanProtocol};
use md5::{Digest, Md5};
use thiserror::Error;

use crate::now_us;
use crate::state::SharedState;

const BASE_BACKOFF_MS: u64 = 1_000;
/// Deutlich großzügiger als z. B. der journalctl-Reconnect in
/// `daemon/src/ingestion.rs` (30s) -- empirisch bestätigt (2026-09-29):
/// wiederholte Login-/Mitschnitt-Versuche im 30s-Takt gegen eine echte
/// Fritz!Box drückten den gemessenen Durchsatz von ~100 Mbit/s auf
/// ~22 Mbit/s, vermutlich durch CPU-Last auf der Box. Ein Ziel, das
/// Consumer-Router-Hardware ist statt eines lokalen Subprozesses, verdient
/// einen spürbar zurückhaltenderen Backoff-Deckel.
const MAX_BACKOFF_MS: u64 = 300_000;
/// Mindestlaufzeit einer Mitschnitt-Session, ab der der Backoff nach ihrem
/// Ende zurückgesetzt wird (dieselbe Idee wie `MIN_STABLE_RUN` in
/// `daemon/src/ingestion.rs`).
const MIN_STABLE_RUN: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
enum CaptureError {
    #[error("HTTP-Anfrage an die Fritz!Box fehlgeschlagen: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Login-Antwort der Fritz!Box enthielt keine <Challenge>")]
    NoChallenge,
    #[error("Login-Antwort der Fritz!Box enthielt keine <SID>")]
    NoSid,
    #[error("Login fehlgeschlagen (SID bleibt 0000000000000000) -- Zugangsdaten/Rechte prüfen")]
    LoginDenied,
    #[error(
        "PBKDF2-Login wird noch nicht unterstützt (Fritz!OS 7.25+, Challenge begann mit \"2$\")"
    )]
    Pbkdf2NotSupported,
    #[error("unerwartete pcap-Magic-Bytes im Mitschnitt-Stream, erste Bytes: {0}")]
    UnknownPcapMagic(String),
}

/// Formatiert die ersten Bytes eines unerwarteten Streams zur Diagnose: Hex
/// und, soweit druckbar, als Text -- damit sich z. B. eine HTML-Fehlerseite
/// (abgelaufene Session, falsche Interface-Kennung) sofort im Log von einem
/// echten, nur anders aufgebauten Binärformat unterscheiden lässt.
fn preview_bytes(bytes: &[u8]) -> String {
    let sample = &bytes[..bytes.len().min(32)];
    let hex: String = sample.iter().map(|b| format!("{b:02x} ")).collect();
    let text: String = sample
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
        .collect();
    format!("hex=[{}] text=\"{}\"", hex.trim_end(), text)
}

/// Liest die Passwortdatei (eine Zeile, kein Encoding). Getrennt von
/// [`CaptureError`], weil ein Lesefehler hier den Task dauerhaft beendet
/// statt mit Backoff neu zu versuchen -- eine fehlende/unlesbare Datei
/// behebt sich nicht von selbst zwischen zwei Versuchen.
fn read_password(path: &str) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(|s| s.trim().to_string())
}

fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// AVM-Challenge-Response: `challenge + "-" + password`, als UTF-16LE-Bytes,
/// MD5, hex-kodiert (Kleinbuchstaben). Reine Funktion, unabhängig von HTTP,
/// daher isoliert testbar.
fn challenge_response(challenge: &str, password: &str) -> String {
    let combined = format!("{challenge}-{password}");
    let mut utf16le_bytes = Vec::with_capacity(combined.len() * 2);
    for unit in combined.encode_utf16() {
        utf16le_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let digest = Md5::digest(&utf16le_bytes);
    let hash_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{challenge}-{hash_hex}")
}

async fn login(
    client: &reqwest::Client,
    config: &FritzboxConfig,
    password: &str,
) -> Result<String, CaptureError> {
    let challenge_url = format!("http://{}/login_sid.lua", config.host);
    let body = client.get(&challenge_url).send().await?.text().await?;
    let challenge = extract_xml_tag(&body, "Challenge").ok_or(CaptureError::NoChallenge)?;
    if challenge.starts_with("2$") {
        return Err(CaptureError::Pbkdf2NotSupported);
    }
    let response = challenge_response(challenge, password);

    let login_url = format!(
        "http://{}/login_sid.lua?username={}&response={response}",
        config.host, config.username
    );
    let body = client.get(&login_url).send().await?.text().await?;
    let sid = extract_xml_tag(&body, "SID").ok_or(CaptureError::NoSid)?;
    if sid == "0000000000000000" {
        return Err(CaptureError::LoginDenied);
    }
    Ok(sid.to_string())
}

/// Liest den pcap-Global-Header (24 Byte) einmalig und danach fortlaufend
/// Record-Header (16 Byte) + Rohframe aus einem in beliebig großen Häppchen
/// eintreffenden Byte-Strom. Nur Standard-Mikrosekunden-Auflösung wird
/// erkannt (kein Nanosekunden-Varianten-Magic) -- die tatsächlich genutzte
/// Auflösung spielt für diese Auswertung ohnehin keine Rolle, nur das
/// Byte-Layout muss stimmen.
struct PcapStreamParser {
    buffer: Vec<u8>,
    header_consumed: bool,
    little_endian: bool,
}

impl PcapStreamParser {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            header_consumed: false,
            little_endian: true,
        }
    }

    /// Nimmt neu eingetroffene Bytes auf und gibt alle daraus vollständig
    /// zusammensetzbaren Rohframes zurück. Unvollständige Reste bleiben im
    /// internen Puffer bis zum nächsten Aufruf.
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, CaptureError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        if !self.header_consumed {
            if self.buffer.len() < 24 {
                return Ok(frames);
            }
            self.little_endian = match &self.buffer[0..4] {
                [0xd4, 0xc3, 0xb2, 0xa1] => true,
                [0xa1, 0xb2, 0xc3, 0xd4] => false,
                // AVM-eigene Magic-Number, empirisch gegen eine echte
                // Fritz!Box verifiziert: der Rest des Headers ist bei
                // Big-Endian-Lesart identisch zum Standardformat aufgebaut
                // (Version 2.4, Snaplen, Linktyp Ethernet), auch der erste
                // Paket-Zeitstempel ergibt nur big-endian ein plausibles
                // aktuelles Datum. Keine offizielle libpcap-Magic, aber
                // strukturell dasselbe Format.
                [0xa1, 0xb2, 0xcd, 0x34] => false,
                _ => return Err(CaptureError::UnknownPcapMagic(preview_bytes(&self.buffer))),
            };
            self.buffer.drain(0..24);
            self.header_consumed = true;
        }

        loop {
            if self.buffer.len() < 16 {
                break;
            }
            let incl_len = self.read_u32(&self.buffer[8..12]) as usize;
            let total = 16 + incl_len;
            if self.buffer.len() < total {
                break;
            }
            frames.push(self.buffer[16..total].to_vec());
            self.buffer.drain(0..total);
        }
        Ok(frames)
    }

    fn read_u32(&self, bytes: &[u8]) -> u32 {
        let array: [u8; 4] = bytes.try_into().unwrap_or([0; 4]);
        if self.little_endian {
            u32::from_le_bytes(array)
        } else {
            u32::from_be_bytes(array)
        }
    }
}

/// Header-Felder eines IPv4-TCP/UDP-Flows, aus einem einzelnen
/// Ethernet-Frame gelesen.
struct ParsedFlow {
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    dst_port: u16,
    protocol: LanProtocol,
}

/// Parst ein einzelnes Ethernet-Frame zu einem IPv4-Flow, falls es sich um
/// IPv4 mit TCP oder UDP handelt. Alles andere (ARP, IPv6, andere
/// L4-Protokolle, zu kurze Frames) liefert `None` statt eines Fehlers -- ein
/// einzelnes nicht auswertbares Paket ist kein Grund, den Mitschnitt
/// abzubrechen (Regel 16).
fn parse_ipv4_flow(frame: &[u8]) -> Option<ParsedFlow> {
    if frame.len() < 14 {
        return None;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != 0x0800 {
        return None; // nur IPv4, siehe Moduldoc
    }
    let ip = &frame[14..];
    if ip.len() < 20 {
        return None;
    }
    if ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    if ip.len() < ihl {
        return None;
    }
    let protocol = match ip[9] {
        6 => LanProtocol::Tcp,
        17 => LanProtocol::Udp,
        _ => return None,
    };
    let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let dst_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let l4 = &ip[ihl..];
    if l4.len() < 4 {
        return None;
    }
    let dst_port = u16::from_be_bytes([l4[2], l4[3]]);
    Some(ParsedFlow {
        src_ip,
        dst_ip,
        dst_port,
        protocol,
    })
}

/// Ob ein Ziel für die Anzeige uninteressant ist, weil es kein "Verbindung
/// ins Internet"-Ziel ist (internes Netzwerk-Rauschen, Broadcast/Multicast).
fn is_uninteresting_destination(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_multicast() || ip.is_broadcast()
}

/// Sucht die MAC-Adresse eines bekannten LAN-Geräts zu einer beobachteten
/// Quell-IP. `None`, wenn die IP keinem aktuell bekannten Gerät gehört
/// (z. B. die Fritz!Box selbst, oder ein Gerät, das `lan_devices` noch
/// nicht erfasst hat) -- solche Flows werden verworfen, siehe Moduldoc.
fn lookup_mac(state: &SharedState, ip: Ipv4Addr) -> Option<String> {
    let ip_text = ip.to_string();
    state
        .latest_lan_devices()
        .iter()
        .find(|device| device.ip == ip_text)
        .map(|device| device.mac.clone())
}

async fn run_one_capture_session(
    client: &reqwest::Client,
    config: &FritzboxConfig,
    password: &str,
    state: &SharedState,
) -> Result<(), CaptureError> {
    let sid = login(client, config, password).await?;
    let capture_url = format!(
        "http://{}/cgi-bin/capture_notimeout?ifaceorminor={}&snaplen=&capture=Start&sid={sid}",
        config.host, config.capture_iface
    );
    let response = client.get(&capture_url).send().await?;
    let mut stream = response.bytes_stream();
    let mut parser = PcapStreamParser::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&chunk)? {
            let Some(flow) = parse_ipv4_flow(&frame) else {
                continue;
            };
            if is_uninteresting_destination(flow.dst_ip) {
                continue;
            }
            let Some(src_mac) = lookup_mac(state, flow.src_ip) else {
                continue;
            };
            state.publish_lan_flow(LanFlowEvent {
                src_mac,
                dst_ip: flow.dst_ip.to_string(),
                dst_port: flow.dst_port,
                protocol: flow.protocol,
                timestamp_us: now_us(),
            });
        }
    }
    Ok(())
}

/// Haupt-Schleife: meldet sich an, liest den Mitschnitt-Stream, versucht bei
/// jedem Abbruch (Verbindungsverlust, Login-Fehler, Fritz!Box-Neustart) mit
/// exponentiellem Backoff erneut -- außer bei [`CaptureError::Pbkdf2NotSupported`],
/// das behebt sich nicht durch Wiederholen.
pub async fn run_fritzbox_capture(config: FritzboxConfig, state: Arc<SharedState>) {
    if !config.enabled {
        return;
    }

    let password = match read_password(&config.password_file) {
        Ok(password) => password,
        Err(err) => {
            tracing::error!(
                pfad = %config.password_file,
                fehler = %err,
                "Fritz!Box-Passwortdatei nicht lesbar, Paketmitschnitt bleibt aus"
            );
            return;
        }
    };

    let client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(err) => {
            tracing::error!(fehler = %err, "HTTP-Client für Fritz!Box-Paketmitschnitt konnte nicht gebaut werden");
            return;
        }
    };

    let mut backoff_ms = BASE_BACKOFF_MS;
    loop {
        let started_at = tokio::time::Instant::now();
        let outcome = run_one_capture_session(&client, &config, &password, &state).await;

        if started_at.elapsed() >= MIN_STABLE_RUN {
            backoff_ms = BASE_BACKOFF_MS;
        }

        match outcome {
            Ok(()) => {
                tracing::warn!(
                    backoff_ms,
                    "Fritz!Box-Mitschnitt-Verbindung beendet, starte nach Backoff neu"
                );
            }
            Err(CaptureError::Pbkdf2NotSupported) => {
                tracing::error!(
                    "Fritz!Box verlangt PBKDF2-Login, das logsentry noch nicht unterstützt -- \
                     Paketmitschnitt bleibt dauerhaft aus, kein wiederholter Versuch"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    fehler = %err,
                    backoff_ms,
                    "Fritz!Box-Paketmitschnitt fehlgeschlagen, starte nach Backoff neu"
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = backoff_ms.saturating_mul(2).min(MAX_BACKOFF_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_xml_tag_findet_inhalt_zwischen_tags() {
        let xml = "<SessionInfo><SID>abc123</SID><Challenge>1234567z</Challenge></SessionInfo>";
        assert_eq!(extract_xml_tag(xml, "SID"), Some("abc123"));
        assert_eq!(extract_xml_tag(xml, "Challenge"), Some("1234567z"));
        assert_eq!(extract_xml_tag(xml, "Fehlt"), None);
    }

    #[test]
    fn challenge_response_stimmt_mit_bekanntem_avm_beispiel_ueberein() {
        // Bekanntes Beispiel aus der AVM-Dokumentation zum
        // Challenge-Response-Verfahren (login_sid.lua, MD5-Variante).
        let result = challenge_response("1234567z", "geheim");
        assert!(result.starts_with("1234567z-"));
        assert_eq!(result.len(), "1234567z-".len() + 32); // MD5-Hex ist 32 Zeichen lang
    }

    #[test]
    fn challenge_response_ist_deterministisch() {
        let a = challenge_response("abc", "pw");
        let b = challenge_response("abc", "pw");
        assert_eq!(a, b);
    }

    #[test]
    fn pcap_parser_liefert_nichts_vor_vollstaendigem_header() {
        let mut parser = PcapStreamParser::new();
        let frames = parser.feed(&[0xd4, 0xc3, 0xb2, 0xa1, 0, 0]).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn pcap_parser_lehnt_unbekannte_magic_bytes_ab() {
        let mut parser = PcapStreamParser::new();
        let mut header = vec![0xff, 0xff, 0xff, 0xff];
        header.extend_from_slice(&[0u8; 20]);
        assert!(parser.feed(&header).is_err());
    }

    #[test]
    fn pcap_parser_akzeptiert_die_avm_eigene_magic_number() {
        // Echte, per journalctl beobachtete erste Bytes eines Fritz!Box-
        // Mitschnitt-Streams (siehe Kommentar an der Magic-Prüfung):
        // AVM-Magic a1 b2 cd 34, Rest des Headers big-endian (Version 2.4,
        // Snaplen 0x800, Linktyp 1 = Ethernet).
        let mut stream: Vec<u8> = vec![
            0xa1, 0xb2, 0xcd, 0x34, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x01,
        ];
        // Ein minimaler Record: ts_sec/ts_usec (big-endian) egal fuer den
        // Test, incl_len=orig_len=4, ein 4-Byte-"Frame".
        stream.extend_from_slice(&0x6abc2cb5u32.to_be_bytes());
        stream.extend_from_slice(&0x000911b8u32.to_be_bytes());
        stream.extend_from_slice(&4u32.to_be_bytes());
        stream.extend_from_slice(&4u32.to_be_bytes());
        stream.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let mut parser = PcapStreamParser::new();
        let frames = parser.feed(&stream).expect("AVM-Magic muss akzeptiert werden");
        assert_eq!(frames, vec![vec![0xAA, 0xBB, 0xCC, 0xDD]]);
    }

    #[test]
    fn pcap_parser_extrahiert_ein_vollstaendiges_frame_ueber_zwei_haeppchen() {
        let mut parser = PcapStreamParser::new();
        let mut global_header = vec![0xd4, 0xc3, 0xb2, 0xa1];
        global_header.extend_from_slice(&[0u8; 20]);

        let payload = vec![0xAAu8; 10];
        let mut record_header = Vec::new();
        record_header.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
        record_header.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        record_header.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // incl_len
        record_header.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // orig_len

        let mut first_chunk = global_header;
        first_chunk.extend_from_slice(&record_header);
        first_chunk.extend_from_slice(&payload[..5]);
        let second_chunk = payload[5..].to_vec();

        assert!(parser.feed(&first_chunk).unwrap().is_empty());
        let frames = parser.feed(&second_chunk).unwrap();
        assert_eq!(frames, vec![payload]);
    }

    #[test]
    fn parse_ipv4_flow_erkennt_tcp_verbindung() {
        let mut frame = vec![0u8; 14]; // Ethernet-Header, Inhalt irrelevant außer EtherType
        frame[12] = 0x08;
        frame[13] = 0x00; // EtherType IPv4

        let mut ip = vec![0u8; 20];
        ip[0] = 0x45; // Version 4, IHL 5 (20 Byte, keine Optionen)
        ip[9] = 6; // TCP
        ip[12..16].copy_from_slice(&[192, 168, 178, 20]);
        ip[16..20].copy_from_slice(&[93, 184, 216, 34]);
        frame.extend_from_slice(&ip);

        let mut tcp = vec![0u8; 4];
        tcp[2..4].copy_from_slice(&443u16.to_be_bytes());
        frame.extend_from_slice(&tcp);

        let flow = parse_ipv4_flow(&frame).expect("muss als IPv4/TCP erkannt werden");
        assert_eq!(flow.src_ip, Ipv4Addr::new(192, 168, 178, 20));
        assert_eq!(flow.dst_ip, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(flow.dst_port, 443);
        assert_eq!(flow.protocol, LanProtocol::Tcp);
    }

    #[test]
    fn parse_ipv4_flow_ignoriert_arp() {
        let mut frame = vec![0u8; 14];
        frame[12] = 0x08;
        frame[13] = 0x06; // EtherType ARP
        assert!(parse_ipv4_flow(&frame).is_none());
    }

    #[test]
    fn is_uninteresting_destination_erkennt_private_und_multicast_ziele() {
        assert!(is_uninteresting_destination(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_uninteresting_destination(Ipv4Addr::new(224, 0, 0, 1)));
        assert!(!is_uninteresting_destination(Ipv4Addr::new(93, 184, 216, 34)));
    }
}
