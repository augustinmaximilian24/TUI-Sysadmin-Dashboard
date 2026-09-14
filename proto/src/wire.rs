//! Wire-Format v1 zwischen Daemon und Client(s) (GUI, später TUI).
//!
//! Normativ nach `docs/phase6-protokoll.md` Abschnitt 3. Änderungen an
//! bestehenden Feldern brechen das Protokoll und erfordern einen
//! [`PROTOCOL_VERSION`]-Sprung; neue Felder werden mit `#[serde(default)]`
//! ergänzt, damit ältere Clients/Daemons kompatibel bleiben.
//!
//! `proto` hängt bewusst nicht von `logsentry-core` ab (siehe Dokument
//! Abschnitt 1): das Wire-Format ist ein Vertrag gegenüber allen Clients,
//! `core`-Typen dürfen sich frei ändern. Der Daemon bildet in
//! `daemon/src/wire.rs` von `core` auf diese Typen ab.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// Aktuelle Protokollversion. Muss im Handshake exakt übereinstimmen.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximale Länge einer Zeile Client → Daemon in Bytes. Überschreitung:
/// Verbindung trennen (siehe Abschnitt 4, Regel 3).
pub const MAX_CLIENT_LINE_BYTES: usize = 256 * 1024;

/// Maximale Länge einer Zeile Daemon → Client in Bytes. `RecentAnomalies`
/// ist mit Abstand die größte Nachricht.
pub const MAX_SERVER_LINE_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------
// Client → Daemon
// ---------------------------------------------------------------------

/// Eine Nachricht vom Client an den Daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Muss die erste Nachricht der Verbindung sein.
    Hello {
        protocol_version: u16,
        client_name: String,
        client_version: String,
        subscription: Subscription,
    },
    /// Laufendes Abo ändern (z. B. Snapshot-Rate drosseln).
    Subscribe {
        subscription: Subscription,
    },
    /// Rohzeilen rund um einen Zeitpunkt anfordern (Detail-Panel, Export).
    GetContext {
        request_id: u64,
        timestamp_us: u64,
        before: u16,
        after: u16,
        #[serde(default)]
        unit: Option<String>,
    },
    /// Aktionsanfrage. Phase 6: der Daemon antwortet immer mit
    /// `ActionOutcome::Denied(DenyReason::NotAllowed)`.
    Action {
        request_id: u64,
        action: ActionRequest,
    },
    Ping {
        nonce: u64,
    },
}

/// Wunsch-Abo des Clients. Der Daemon klemmt `snapshot_interval_ms` auf
/// `[min_snapshot_interval_ms, 60_000]` aus der Daemon-Konfiguration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Subscription {
    pub snapshots: bool,
    pub anomalies: bool,
    pub snapshot_interval_ms: u32,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            snapshots: true,
            anomalies: true,
            snapshot_interval_ms: 1000,
        }
    }
}

// ---------------------------------------------------------------------
// Daemon → Client
// ---------------------------------------------------------------------

/// Eine Nachricht vom Daemon an den Client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Antwort auf `ClientMessage::Hello`.
    Hello {
        protocol_version: u16,
        daemon_version: String,
        hostname: String,
        /// Zufällig pro Daemon-Start; ändert sich beim Reconnect nach einem
        /// Daemon-Neustart. Anomalie-`id`s sind nur innerhalb einer Session
        /// eindeutig.
        session_id: u64,
        allowed_actions: Vec<ActionKind>,
        dry_run: bool,
    },
    /// Einmalig direkt nach `Hello`: die letzten N Anomalien dieser Session
    /// (auch als leere Liste, wenn noch keine aufgetreten sind).
    RecentAnomalies {
        anomalies: Vec<AnomalyEvent>,
    },
    Snapshot(Snapshot),
    Anomaly(AnomalyEvent),
    Context(ContextReply),
    ActionResult {
        request_id: u64,
        outcome: ActionOutcome,
    },
    Pong {
        nonce: u64,
    },
    /// Protokollfehler; die Verbindung bleibt bestehen, außer der Daemon
    /// sendet danach `Goodbye`.
    Error {
        code: ErrorCode,
        message: String,
        #[serde(default)]
        request_id: Option<u64>,
    },
    /// Letzte Nachricht vor dem Trennen durch den Daemon.
    Goodbye {
        reason: GoodbyeReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedMessage,
    UnknownMessage,
    HelloExpected,
    Lagged { missed: u64 },
    ContextUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum GoodbyeReason {
    VersionMismatch { expected: u16, got: u16 },
    HelloTimeout,
    TooManyClients { max: u32 },
    LineTooLong,
    Shutdown,
}

// ---------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Snapshot {
    pub timestamp_us: u64,
    pub daemon_uptime_secs: u64,
    pub learning: LearningState,
    pub stats: PipelineStats,
    pub window: WindowStats,
    #[serde(default)]
    pub system: Option<SystemSnapshot>,
    pub replay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LearningState {
    pub active: bool,
    #[serde(default)]
    pub remaining_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PipelineStats {
    pub events: u64,
    pub parse_errors: u64,
    pub dropped_overflow: u64,
    pub templates: u64,
    pub anomalies_emitted: u64,
    pub suppressed: u64,
    pub suppressed_learning: u64,
    pub events_per_sec: f64,
    pub clients: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WindowStats {
    pub window_secs: u32,
    pub entropy_bits: f64,
    pub entropy_z: f64,
    pub events_in_window: u64,
    pub distinct_templates: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SystemSnapshot {
    pub timestamp_us: u64,
    pub cpu: CpuSnapshot,
    pub memory: MemorySnapshot,
    pub load: LoadSnapshot,
    pub disks: Vec<DiskSnapshot>,
    pub temperatures: Vec<TemperatureSnapshot>,
    pub units: Vec<UnitStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CpuSnapshot {
    pub global_usage_percent: f32,
    pub per_core_usage_percent: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemorySnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LoadSnapshot {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiskSnapshot {
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TemperatureSnapshot {
    pub label: String,
    pub celsius: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UnitStatus {
    pub name: String,
    pub active_state: String,
    pub sub_state: String,
}

// ---------------------------------------------------------------------
// Anomalie
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnomalyEvent {
    /// Monoton steigend pro Session, ab 1.
    pub id: u64,
    pub timestamp_us: u64,
    pub template_id: u64,
    pub template_text: String,
    /// Auslösende Rohzeile, auf 1024 Byte gekürzt (Daemon kürzt, nicht der
    /// Client), `[…]` angehängt wenn gekürzt.
    pub sample_message: String,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub priority: Option<u8>,
    pub level: AnomalyLevel,
    pub breakdown: ScoreBreakdown,
    pub suppressed_since_last: u64,
}

/// Schweregrad einer Anomalie. `Normal` aus `core::AnomalyLevel` wird nie
/// über das Protokoll gesendet — dort gibt es schlicht kein `AnomalyEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyLevel {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ScoreBreakdown {
    pub rate_z: f64,
    pub surprisal_bits: f64,
    pub entropy_z: f64,
    pub rate_component: f64,
    pub surprisal_component: f64,
    pub entropy_component: f64,
    pub combined: f64,
    pub rate_source: RateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    SlotBaseline,
    AnySlotBaseline,
    ShortTerm,
    None,
}

// ---------------------------------------------------------------------
// Kontext
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextReply {
    pub request_id: u64,
    pub lines: Vec<ContextLine>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextLine {
    pub timestamp_us: u64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub priority: Option<u8>,
    /// Bereits UTF-8-lossy konvertiert; die Byte-Array-Variante aus Phase 1
    /// bleibt intern im Daemon.
    pub message: String,
}

// ---------------------------------------------------------------------
// Aktionen (Regel 8: nur typisierte Parameter, niemals Kommandostrings)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    RestartUnit,
    StopUnit,
    TerminateProcess,
    BlockIp,
    MuteAnomaly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionRequest {
    RestartUnit {
        unit: UnitName,
    },
    StopUnit {
        unit: UnitName,
    },
    TerminateProcess {
        pid: u32,
        /// Sekunden bis SIGKILL nach SIGTERM. Der Daemon klemmt auf
        /// `[1, 60]`.
        grace_secs: u16,
    },
    BlockIp {
        ip: IpAddr,
        /// Der Daemon klemmt auf `[60, 604_800]` (7 Tage).
        duration_secs: u32,
    },
    MuteAnomaly {
        template_id: u64,
        #[serde(default)]
        unit: Option<String>,
        scope: MuteScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuteScope {
    OneHour,
    OneDay,
    Permanent,
}

/// Ein syntaktisch geprüfter systemd-Unit-Name.
///
/// Die Prüfung ist eine Vorprüfung auf Zeichensatz und bekannten
/// Typ-Suffix — sie ersetzt nicht den Abgleich gegen die Allow-List im
/// Daemon (Regel 9). Erzeugung nur über [`UnitName::parse`], damit über das
/// Protokoll niemals ein unvalidierter Name den Client verlässt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitName(String);

/// Von systemd verwendete Unit-Typ-Suffixe, die für die hier erlaubten
/// Aktionen (Neustart/Stop) infrage kommen.
const ALLOWED_UNIT_SUFFIXES: &[&str] = &[
    "service", "socket", "timer", "mount", "target", "path", "slice", "scope",
];

impl UnitName {
    /// Prüft `name` gegen das systemd-Namensmuster und einen bekannten
    /// Typ-Suffix. Erlaubte Zeichen: `[A-Za-z0-9:_.@\-]`, dazu `\` für von
    /// systemd escapte Namen (z. B. `getty@tty1.service`).
    pub fn parse(name: &str) -> Result<Self, ProtoError> {
        if name.is_empty() {
            return Err(ProtoError::InvalidUnitName {
                name: name.to_string(),
                reason: "darf nicht leer sein",
            });
        }
        if name.len() > 255 {
            return Err(ProtoError::InvalidUnitName {
                name: name.to_string(),
                reason: "länger als 255 Zeichen",
            });
        }
        let chars_ok = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\'));
        if !chars_ok {
            return Err(ProtoError::InvalidUnitName {
                name: name.to_string(),
                reason: "enthält unzulässige Zeichen",
            });
        }
        let suffix = name.rsplit('.').next().unwrap_or("");
        if !ALLOWED_UNIT_SUFFIXES.contains(&suffix) {
            return Err(ProtoError::InvalidUnitName {
                name: name.to_string(),
                reason: "unbekannter oder fehlender Unit-Typ-Suffix",
            });
        }
        Ok(Self(name.to_string()))
    }

    /// Der geprüfte Name als `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UnitName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for UnitName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UnitName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        UnitName::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ActionOutcome {
    Accepted,
    Completed { message: String, dry_run: bool },
    Denied { reason: DenyReason },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DenyReason {
    NotAllowed,
    RateLimited { retry_after_secs: u32 },
    Unauthorized,
    InvalidParameter { message: String },
}
