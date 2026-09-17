# Phase 6 – Daemon & Protokoll: Entwurf

Stand: Entwurf (Fable 5.1). Umsetzung erfolgt mit Sonnet 5 nach diesem
Dokument. Abschnitte 3–5 sind normativ; wer davon abweicht, ändert erst
dieses Dokument.

## 1. Entscheidungen im Überblick

| Frage | Entscheidung | Begründung |
|---|---|---|
| Hängt `proto/` von `core/` ab? | **Nein.** Eigene Wire-Typen, Daemon mappt per `From<core::…>`. | Das Wire-Format ist ein Vertrag gegenüber GUI und späterem TUI; `core`-Typen dürfen sich ohne Protokollbruch ändern. Clients ziehen so weder `redb`/`rusqlite` noch `sysinfo` als Abhängigkeit. |
| Framing | JSON-Lines, `\n`-getrennt, kein Envelope pro Nachricht. | Versionsverhandlung passiert einmal im Handshake; ein `version`-Feld in jeder Nachricht wäre totes Gewicht. |
| Versionierung | `PROTOCOL_VERSION: u16 = 1`, strikt gleich im Handshake. Erweiterungen innerhalb einer Version nur durch **optionale** Felder (`#[serde(default)]`). | Einfach, prüfbar, kein Halbkompatibilitäts-Sumpf. |
| Snapshot-Verteilung | `tokio::sync::watch<Arc<Snapshot>>` | Latest-value-Semantik: ein langsamer Client bekommt den neuesten Stand, es staut sich nichts. |
| Anomalie-Verteilung | `tokio::sync::broadcast<Arc<AnomalyEvent>>(256)` | Lag ist erkennbar (`RecvError::Lagged(n)`) und wird dem Client als Zähler gemeldet, nicht verschwiegen. |
| Antworten (Context, ActionResult, Pong) | pro Client `mpsc::channel(32)` | Bounded (Regel 17). |
| Aktionen | Enum mit typisierten Parametern schon jetzt vollständig definiert; Daemon lehnt in Phase 6 alles mit `Denied(NotAllowed)` ab und schreibt ins Audit-Log. | Phase 8 ändert dann nur den Executor, nicht das Protokoll. |
| Client-Logik | `proto::client` hinter Feature `client` (zieht `tokio`). | GUI (Phase 7) und TUI (Phase 10) teilen Reconnect, Framing und Handshake. |
| Socket-Gruppe setzen | `nix` (Features `user`, `fs`) für `getgrnam` + `chown`. | `std::os::unix::fs::chown` kann keine Gruppe nach Namen auflösen. Einzige neue Abhängigkeit dieser Phase. |

## 2. Neue Abhängigkeiten

| Crate | Wo | Begründung |
|---|---|---|
| `nix = { version = "0.29", features = ["user", "fs"] }` | `daemon` | Gruppenname → GID, `fchmodat`/`chown` auf den Socket. |
| `tokio` (bereits Workspace) | `proto` nur mit Feature `client` | Client-Task, `UnixStream`, Backoff-Timer. |
| `thiserror` (bereits Workspace) | `proto` | Fehlertypen für Framing/Handshake (Regel: Bibliotheksteile). |

Nicht aufnehmen: `tokio-util` (Codec) – ein `BufReader::read_line` mit Längenprüfung reicht; `bytes`, `postcard`/`bincode` – JSON ist Vorgabe und für die Debug-Barkeit (`socat`, `jq`) gewollt.

## 3. Wire-Typen (normativ)

Alle Enums: `#[serde(tag = "type", rename_all = "snake_case")]`. Alle Structs: `#[serde(rename_all = "snake_case")]`, `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]`. Felder, die nach v1 hinzukommen, bekommen `#[serde(default)]` – niemals Felder entfernen oder umbenennen ohne Versionssprung.

```rust
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximale Länge einer Zeile Client → Daemon. Überschreitung: Verbindung trennen.
pub const MAX_CLIENT_LINE_BYTES: usize = 256 * 1024;
/// Maximale Länge einer Zeile Daemon → Client (RecentAnomalies ist die größte).
pub const MAX_SERVER_LINE_BYTES: usize = 4 * 1024 * 1024;

// ---------- Client → Daemon ----------

pub enum ClientMessage {
    /// Muss die erste Nachricht sein. Kommt sie nicht innerhalb `hello_timeout`,
    /// trennt der Daemon.
    Hello {
        protocol_version: u16,
        client_name: String,      // "logsentry-gui", "logsentry-tui"
        client_version: String,   // Cargo-Version des Clients
        subscription: Subscription,
    },
    /// Abo ändern (z. B. Snapshot-Rate drosseln, wenn Fenster minimiert).
    Subscribe { subscription: Subscription },
    /// Rohzeilen rund um einen Zeitpunkt (Detail-Panel, Export).
    GetContext {
        request_id: u64,
        timestamp_us: u64,
        /// Zeilen davor/danach; Daemon klemmt auf `context_max_lines`.
        before: u16,
        after: u16,
        /// Optional auf eine Unit einschränken.
        unit: Option<String>,
    },
    /// Aktionsanfrage. Phase 6: wird immer mit `Denied(NotAllowed)` beantwortet.
    Action { request_id: u64, action: ActionRequest },
    Ping { nonce: u64 },
}

pub struct Subscription {
    pub snapshots: bool,          // Default true
    pub anomalies: bool,          // Default true
    /// Wunsch-Intervall; Daemon klemmt auf [min_snapshot_interval_ms, 60_000].
    pub snapshot_interval_ms: u32,
}

// ---------- Daemon → Client ----------

pub enum ServerMessage {
    Hello {
        protocol_version: u16,
        daemon_version: String,
        hostname: String,
        /// Zufällig pro Daemon-Start. Ändert es sich beim Reconnect, wurde
        /// der Daemon neu gestartet – Anomalie-IDs sind nur pro Session eindeutig.
        session_id: u64,
        /// Was der Daemon laut Allow-List ausführen würde (Phase 6: leer).
        allowed_actions: Vec<ActionKind>,
        dry_run: bool,
    },
    /// Beim Verbinden einmalig: die letzten N Anomalien dieser Session.
    RecentAnomalies { anomalies: Vec<AnomalyEvent> },
    Snapshot(Snapshot),
    Anomaly(AnomalyEvent),
    Context(ContextReply),
    ActionResult { request_id: u64, outcome: ActionOutcome },
    Pong { nonce: u64 },
    /// Protokollfehler, Verbindung bleibt bestehen (außer bei `code = Fatal`).
    Error { code: ErrorCode, message: String, request_id: Option<u64> },
    /// Letzte Nachricht vor dem Trennen durch den Daemon.
    Goodbye { reason: GoodbyeReason },
}

pub enum ErrorCode {
    /// Zeile war kein gültiges JSON – gilt als Fatal, Daemon trennt.
    MalformedMessage,
    /// `type` unbekannt oder Felder fehlen – ignoriert, Verbindung bleibt.
    UnknownMessage,
    /// Nachricht vor Hello.
    HelloExpected,
    /// Client hat `missed` Anomalie-Events verpasst (Broadcast-Lag).
    Lagged { missed: u64 },
    /// GetContext außerhalb des vorgehaltenen Fensters.
    ContextUnavailable,
}

pub enum GoodbyeReason {
    VersionMismatch { expected: u16, got: u16 },
    HelloTimeout,
    TooManyClients { max: u32 },
    LineTooLong,
    Shutdown,
}

// ---------- Snapshot ----------

pub struct Snapshot {
    pub timestamp_us: u64,
    pub daemon_uptime_secs: u64,
    pub learning: LearningState,
    pub stats: PipelineStats,
    pub window: WindowStats,
    /// None, solange sysmon noch keinen ersten Lauf hatte.
    pub system: Option<SystemSnapshot>,
    /// Ob der Daemon gerade nachliest (Replay) statt live folgt.
    pub replay: bool,
}

pub struct LearningState {
    pub active: bool,
    pub remaining_secs: Option<u64>,
}

pub struct PipelineStats {
    pub events: u64,
    pub parse_errors: u64,
    /// Kanal-Überlauf Ingestion → Analyse (RingReceiver::dropped_count).
    pub dropped_overflow: u64,
    pub templates: u64,
    pub anomalies_emitted: u64,
    pub suppressed: u64,
    pub suppressed_learning: u64,
    /// Ereignisrate über das Analysefenster.
    pub events_per_sec: f64,
    /// Anzahl verbundener Clients (für den Kopfbereich).
    pub clients: u32,
}

pub struct WindowStats {
    pub window_secs: u32,
    pub entropy_bits: f64,
    pub entropy_z: f64,
    pub events_in_window: u64,
    pub distinct_templates: u64,
}

// SystemSnapshot, CpuSnapshot, MemorySnapshot, LoadSnapshot, DiskSnapshot,
// TemperatureSnapshot, UnitStatus: feldgleich zu core::system, aber eigene
// Typen in proto. active_state/sub_state bleiben Strings (systemd-Vokabular
// ist offen, kein Enum erfinden).

// ---------- Anomalie ----------

pub struct AnomalyEvent {
    /// Monoton steigend pro Session, ab 1.
    pub id: u64,
    pub timestamp_us: u64,
    pub template_id: u64,
    /// Maskierter Template-Text zur Anzeige (aus der Template-Registry).
    pub template_text: String,
    /// Auslösende Rohzeile, auf 1024 Byte gekürzt, `[…]` angehängt wenn gekürzt.
    pub sample_message: String,
    pub unit: Option<String>,
    pub pid: Option<u32>,
    /// Journal-PRIORITY 0..7.
    pub priority: Option<u8>,
    pub level: AnomalyLevel,          // Info | Warn | Critical (Normal wird nie gesendet)
    pub breakdown: ScoreBreakdown,    // feldgleich zu core, rate_source als Enum
    pub suppressed_since_last: u64,
}

pub enum RateSource { SlotBaseline, AnySlotBaseline, ShortTerm, None }

// ---------- Kontext ----------

pub struct ContextReply {
    pub request_id: u64,
    pub lines: Vec<ContextLine>,
    /// true, wenn `before`/`after` geklemmt wurden oder das Fenster nicht reicht.
    pub truncated: bool,
}

pub struct ContextLine {
    pub timestamp_us: u64,
    pub unit: Option<String>,
    pub pid: Option<u32>,
    pub priority: Option<u8>,
    pub message: String,   // Nicht-UTF-8 bereits lossy konvertiert
}

// ---------- Aktionen (Regel 8: nur typisierte Parameter) ----------

pub enum ActionKind { RestartUnit, StopUnit, TerminateProcess, BlockIp, MuteAnomaly }

pub enum ActionRequest {
    RestartUnit { unit: UnitName },
    StopUnit { unit: UnitName },
    TerminateProcess {
        pid: u32,
        /// Sekunden bis SIGKILL nach SIGTERM; Daemon klemmt auf [1, 60].
        grace_secs: u16,
    },
    BlockIp {
        ip: std::net::IpAddr,
        /// Daemon klemmt auf [60, 86_400 * 7].
        duration_secs: u32,
    },
    MuteAnomaly {
        template_id: u64,
        unit: Option<String>,
        scope: MuteScope,
    },
}

pub enum MuteScope { OneHour, OneDay, Permanent }

/// systemd-Unit-Name, syntaktisch geprüft: `[A-Za-z0-9:_.@\-\\]{1,255}` mit
/// Suffix aus {service, socket, timer, mount, target, path, slice, scope}.
/// Nur Vorprüfung – der Daemon prüft zusätzlich gegen die Allow-List.
pub struct UnitName(String);   // Konstruktor: UnitName::parse(&str) -> Result<_, ProtoError>

pub enum ActionOutcome {
    Accepted,
    Completed { message: String, dry_run: bool },
    Denied { reason: DenyReason },
    Failed { message: String },
}

pub enum DenyReason {
    NotAllowed,                          // nicht in Allow-List / Phase 6 immer
    RateLimited { retry_after_secs: u32 },
    Unauthorized,                        // polkit hat abgelehnt
    InvalidParameter { message: String },
}
```

Hinweise zur Abbildung:
- `core::TemplateId` → `u64`. Falls `TemplateId` kein `u64`-Newtype ist, ein `From` in `daemon`, nicht das Wire-Format anpassen.
- `AnomalyLevel::Normal` existiert im Protokoll nicht; der Daemon sendet nur `Info`/`Warn`/`Critical`.
- `sample_message` und `ContextLine::message` kommen bereits UTF-8-lossy an; die Byte-Array-Variante aus Phase 1 bleibt im Daemon.

## 4. Verbindungsablauf (normativ)

```
Client                                   Daemon
  │── connect ─────────────────────────────▶│  peer_cred (uid/gid) merken → Audit
  │                                         │  clients >= max? → Goodbye(TooManyClients), close
  │── Hello{v, name, subscription} ────────▶│  kein Hello binnen hello_timeout → Goodbye(HelloTimeout)
  │                                         │  v != PROTOCOL_VERSION → Goodbye(VersionMismatch)
  │◀─ Hello{session_id, allowed_actions…} ──│
  │◀─ RecentAnomalies{…} ───────────────────│  (auch wenn leer)
  │◀─ Snapshot ─────────────────────────────│  sofort, danach im Abo-Intervall
  │◀─ Anomaly ────────────── (Push) ────────│
  │── GetContext / Action / Ping ──────────▶│
  │◀─ Context / ActionResult / Pong ────────│
```

Regeln:
1. **Ohne Hello passiert nichts.** Andere Nachrichten davor → `Error(HelloExpected)`, Verbindung bleibt bis zum Timeout.
2. **Malformed JSON** → `Error(MalformedMessage)` + close. **Unbekannter `type`** → `Error(UnknownMessage)`, weiter (Vorwärtskompatibilität: erst in `serde_json::Value` parsen, `type` prüfen, dann `from_value`).
3. **Zeile > MAX_CLIENT_LINE_BYTES** → `Goodbye(LineTooLong)` + close. Nicht erst puffern und dann prüfen: `read_line` mit Limit abbrechen.
4. **Snapshot-Rate** ist Client-Wunsch, vom Daemon auf `[min_snapshot_interval_ms, 60000]` geklemmt. Der Client-Task hält einen `tokio::time::interval` und liest bei jedem Tick den aktuellen `watch`-Wert – kein Snapshot wird je gequeued.
5. **Broadcast-Lag** → `Error(Lagged{missed})`, weiter. Der Client zeigt „n Anomalien verpasst" im Kopfbereich.
6. **Shutdown** (SIGTERM/SIGINT): an alle Clients `Goodbye(Shutdown)`, 500 ms Gnadenfrist zum Flushen, dann Socket-Datei entfernen.
7. **Kein serverseitiger Idle-Timeout** nach erfolgreichem Hello. Tote Clients erkennt der Daemon am Schreibfehler (Snapshot-Push). Ping/Pong ist rein clientseitig für RTT/Liveness.
8. **Peer-Credentials** (`UnixStream::peer_cred()`) werden beim Connect geloggt und dem Client-Task mitgegeben – Phase 8 schreibt sie ins Audit-Log. Der Client sendet seine Identität nie selbst.

## 5. Socket, Rechte, Fehlerbilder

Konfiguration (neue Sektion, mit Defaults im Config-Struct):

```toml
[socket]
path = "/run/logsentry/logsentry.sock"
group = "logsentry"
mode = 0o660
max_clients = 8
snapshot_interval_ms = 1000
min_snapshot_interval_ms = 250
hello_timeout_ms = 5000
recent_anomalies = 200          # Ring für RecentAnomalies
context_lines = 2000            # Ring für GetContext (Rohzeilen)
context_max_lines = 200         # Klemme für before+after
```

Daemon-Start:
1. Verzeichnis existiert? Sonst anlegen mit 0750 (im Produktivbetrieb legt es Phase 9 per `RuntimeDirectory=` an; das Anlegen hier ist Fallback für den Dev-Betrieb).
2. Socket-Datei vorhanden → Verbindungsversuch. `ECONNREFUSED` = verwaist → unlink. Erfolg = zweite Instanz → Abbruch mit klarer Meldung.
3. `umask(0o117)` setzen, `bind`, umask zurücksetzen. So entsteht 0660 ohne Race zwischen `bind` und `chmod`.
4. Gruppe auflösen (`nix::unistd::Group::from_name`), `chown(path, None, gid)`. Gruppe unbekannt → Abbruch mit Hinweis `groupadd --system logsentry`.
5. CLI-Overrides `--socket-path` und `--socket-group` (Dev-Betrieb unter `$XDG_RUNTIME_DIR/logsentry.sock` ohne Root; `--socket-group` darf dann leer sein = kein chown).

Client-Fehlerbilder (in `proto::client` als eigener Fehlertyp, damit GUI/TUI dieselben Texte zeigen):

| errno | Bedeutung | Anzeige |
|---|---|---|
| `ENOENT` | Daemon läuft nicht | „Daemon nicht erreichbar – `systemctl status logsentry`" |
| `EACCES` | Benutzer nicht in Gruppe | „Kein Zugriff auf den Socket. `sudo usermod -aG logsentry $USER`, danach neu anmelden." |
| `ECONNREFUSED` | verwaiste Socket-Datei | „Daemon abgestürzt? Socket-Datei verwaist – Daemon neu starten." |
| `Goodbye(VersionMismatch)` | Versionen passen nicht | „Protokoll v{got} vs. v{expected} – Daemon und GUI gemeinsam aktualisieren." |

## 6. Client-Modul (`proto::client`, Feature `client`)

```rust
pub struct ClientConfig { pub socket_path: PathBuf, pub client_name: String,
                          pub client_version: String, pub subscription: Subscription }

pub enum ConnectionState { Connecting { attempt: u32 }, Connected { session_id: u64 },
                           Denied(ClientError), Disconnected { retry_in: Duration } }

/// Startet den Verbindungs-Task. Liefert einen Sender für Client→Daemon und
/// einen Receiver für Daemon→Client sowie ein `watch` für den Verbindungsstatus.
pub fn spawn(cfg: ClientConfig)
    -> (mpsc::Sender<ClientMessage>, mpsc::Receiver<ServerMessage>, watch::Receiver<ConnectionState>)
```

- Reconnect: 500 ms · 2^n, Deckel 30 s, ±20 % Jitter, Zähler wird bei erfolgreichem Hello zurückgesetzt.
- Bei `EACCES` **kein** Reconnect-Sturm: Zustand `Denied`, Neuversuch alle 30 s.
- Der Receiver ist bounded (128). Ist er voll, weil die GUI nicht abholt, wird bei Snapshots der älteste **Snapshot** verworfen (Zähler `dropped_local` im Status), Anomalien werden nie lokal verworfen – Snapshots sind ersetzbar, Anomalien nicht.
- Die GUI (Phase 7) hängt an `ServerMessage`-Empfang ein `ctx.request_repaint()` – damit ist Regel 20 (reaktiver Repaint) direkt erfüllt.

## 7. Daemon-Struktur

Neue Dateien in `daemon/src/`:

| Datei | Inhalt |
|---|---|
| `server.rs` | `UnixListener`, Accept-Schleife, Client-Zähler (`Arc<AtomicU32>`), Shutdown-Signal (`watch<bool>`). |
| `client_task.rs` | Ein Task pro Verbindung: Handshake, `select!` über Socket-Lesen / `watch.changed()` / Intervall-Tick / `broadcast.recv()` / Antwort-Kanal. |
| `state.rs` | `SharedState { snapshot: watch::Sender<Arc<Snapshot>>, anomalies: broadcast::Sender<Arc<AnomalyEvent>>, recent: Mutex<VecDeque<Arc<AnomalyEvent>>>, context: Mutex<ContextRing>, next_anomaly_id: AtomicU64, session_id: u64 }` |
| `wire.rs` | `From<core::…> for proto::…`-Impls, Kürzung von `sample_message`. |

Anschlusspunkt: `pipeline.rs` ersetzt das `tracing::warn!` pro Anomalie durch `state.publish_anomaly(...)` (Ring + Broadcast) und ruft alle `snapshot_interval_ms` `state.publish_snapshot(...)`. Der `ContextRing` wird im Hot Path mit jeder Rohzeile gefüllt – `VecDeque` mit fester Kapazität, `pop_front` bei Überlauf, kurzer `Mutex`-Abschnitt. Kein Klonen des Message-Strings über die Kanalgrenze hinaus: die Rohzeile wird als `Arc<str>` gehalten und sowohl vom Ring als auch vom Analysepfad referenziert.

Hot-Path-Kostenrahmen: ein `Arc::clone` + eine `VecDeque`-Operation pro Ereignis. Snapshot-Serialisierung läuft einmal pro Intervall im Server-Task, nicht pro Client (der Client-Task serialisiert den `Arc<Snapshot>` selbst – bei 8 Clients und 1 Hz sind das 8 kleine JSON-Encodes pro Sekunde, vernachlässigbar; ein vorserialisierter `Arc<String>` kann in Phase 9 nachgerüstet werden, falls die Idle-Messung es verlangt).

## 8. Umsetzungsreihenfolge (für Sonnet 5, ein Commit pro Schritt)

1. `proto`: Typen aus Abschnitt 3, `UnitName::parse`, `ProtoError` (thiserror), Feature `client` leer anlegen. Golden-Tests: je Variante eine Zeile in `proto/tests/fixtures/wire_v1.jsonl`, Test liest, deserialisiert, reserialisiert, vergleicht Bytes. **Commit:** „Definiere Wire-Format v1 im proto-Crate"
2. `proto::framing`: `read_frame(reader, max) -> Result<Option<String>, FrameError>` mit Abbruch bei Überlänge (kein vorheriges Puffern), `write_frame`. Tests: exakt an der Grenze, eins darüber, leere Zeile, fehlender `\n` am Ende. **Commit:** „Füge JSON-Lines-Framing mit Längenlimit hinzu"
3. `proto::client`: `spawn`, Backoff, Fehlertyp mit den Texten aus Abschnitt 5. Test mit Mock-Server auf tempdir-Socket. **Commit:** „Implementiere Client-Verbindung mit Reconnect"
4. `daemon`: `[socket]`-Config, `state.rs`, `wire.rs`, `nix`-Abhängigkeit. **Commit:** „Führe geteilten Daemon-Zustand und Wire-Mapping ein"
5. `daemon`: `server.rs` + `client_task.rs`, Socket-Anlage nach Abschnitt 5, Shutdown-Pfad. Integrationstest (tempdir-Socket, Fake-State): Handshake, erster Snapshot, Version-Mismatch → Goodbye, überlange Zeile → close, zweiter Client bei `max_clients = 1` → Goodbye, langsamer Client → `Lagged`. **Commit:** „Starte Unix-Socket-Server im Daemon"
6. `pipeline.rs` an `SharedState` anschließen; `proto/examples/tail.rs` (verbindet, druckt Nachrichten als JSON) für Regel 28 im Replay-Modus laufen lassen. **Commit:** „Verbinde Pipeline mit dem Socket-Server"
7. Idle-Messung Daemon mit 0 und 2 Clients (`tail`-Beispiel), Zahlen in dieses Dokument unter Abschnitt 9 eintragen. **Commit:** „Dokumentiere Idle-Last mit Socket-Server"

Die GUI bleibt in Phase 6 unverändert bis auf `gui/Cargo.toml` (Abhängigkeit auf `logsentry-proto` mit Feature `client`) – die Anbindung ist Phase 7.

## 9. Offene Punkte / Messwerte

- Idle-Last (Schritt 7), gemessen mit `--release` auf dem Ziel-Desktop (Linux Mint, Ryzen 5 7600X), Live-Modus gegen das echte System-Journal, 15 s Messfenster, `snapshot_interval_ms` auf Default (1000):
  - 0 Clients: ≈ 0,0 % CPU (0,01 s CPU-Zeit über 15 s Wandzeit), RSS 22,5 MB
  - 2 Clients (`proto/examples/tail`, Default-Subscription): ≈ 0,0 % CPU, RSS 22,4 MB
  - Beide liegen deutlich unter dem Zielwert aus Regel 27 (< 1 % CPU); der Socket-Server selbst macht in der Praxis keinen messbaren Unterschied zur Baseline ohne Clients. Erneut zu messen, sobald das Aktions-Subsystem (Phase 8) und mehr Clients hinzukommen.
- Ob `Anomaly`-Push zusätzlich einen Debounce für die GUI braucht, entscheidet Phase 7 anhand echter Replays.
- Selbstfilter für Aktions-Logs (Regel 13) ist Phase 8; der `ContextRing` muss diese Zeilen dann trotzdem enthalten (Audit-Sichtbarkeit) – nur die Analyse filtert.
