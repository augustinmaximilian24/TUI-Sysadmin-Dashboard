//! Ein Task pro Client-Verbindung: Handshake, dann `select!` über
//! Socket-Lesen, Snapshot-Intervall, Anomalie-Broadcast und Shutdown.
//!
//! Normativ: `docs/phase6-protokoll.md` Abschnitt 4 (Verbindungsablauf)
//! und Abschnitt 7 (Daemon-Struktur).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use logsentry_proto::{
    read_frame, write_frame, ClientMessage, ContextLine, ContextReply, ErrorCode, FrameError,
    GoodbyeReason, ServerMessage, Subscription, MAX_CLIENT_LINE_BYTES, PROTOCOL_VERSION,
};

use crate::actions::ActionExecutor;
use crate::now_us;
use crate::state::SharedState;

/// Obergrenze für das clientseitig gewünschte Snapshot-Intervall
/// (Abschnitt 4, Regel 4: `[min_snapshot_interval_ms, 60_000]`).
const MAX_SNAPSHOT_INTERVAL_MS: u32 = 60_000;

/// Version des Daemons, wie sie im `Hello` an den Client gemeldet wird.
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Unveränderliche Konfiguration für die Lebensdauer einer
/// Client-Verbindung, aus der Daemon-Konfiguration abgeleitet.
pub struct ClientTaskConfig {
    pub hostname: Arc<str>,
    pub hello_timeout: Duration,
    pub min_snapshot_interval_ms: u32,
    pub context_max_lines: u16,
    /// Aktions-Subsystem (Phase 8): Allow-List/Rate-Limit-Prüfung und
    /// Dispatch für `ClientMessage::Action`.
    pub actions: Arc<ActionExecutor>,
}

/// Ergebnis des Handshakes: entweder eine ausgehandelte `Subscription`,
/// oder die Verbindung wurde bereits beendet (Goodbye/Error+close/EOF).
enum HandshakeOutcome {
    Ready(Subscription),
    Closed,
}

/// Wie eine eingehende Zeile geparst werden konnte (Abschnitt 4, Regel 2:
/// erst auf gültiges JSON prüfen, dann auf einen bekannten `type`, damit
/// künftige Client-Versionen mit neuen Nachrichtentypen nicht die
/// Verbindung sprengen).
enum ParsedLine {
    Message(ClientMessage),
    UnknownMessage,
    MalformedJson,
}

fn parse_client_message(line: &str) -> ParsedLine {
    match serde_json::from_str::<ClientMessage>(line) {
        Ok(msg) => ParsedLine::Message(msg),
        Err(_) => {
            if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                ParsedLine::UnknownMessage
            } else {
                ParsedLine::MalformedJson
            }
        }
    }
}

/// Wird für Verbindungen aufgerufen, die das `max_clients`-Limit
/// überschreiten: sofort `Goodbye(TooManyClients)`, ohne Handshake.
pub async fn reject_too_many(mut stream: UnixStream, max: u32) {
    send_message(
        &mut stream,
        &ServerMessage::Goodbye {
            reason: GoodbyeReason::TooManyClients { max },
        },
    )
    .await;
}

/// Betreut eine einzelne Client-Verbindung von der ersten Byte bis zum
/// Verbindungsende. Gibt zurück, sobald die Verbindung geschlossen ist,
/// gleich aus welchem Grund.
pub async fn run(
    stream: UnixStream,
    state: Arc<SharedState>,
    shutdown: watch::Receiver<bool>,
    config: Arc<ClientTaskConfig>,
) {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let subscription = match do_handshake(&mut reader, &mut writer, &config).await {
        HandshakeOutcome::Ready(subscription) => subscription,
        HandshakeOutcome::Closed => return,
    };

    send_message(
        &mut writer,
        &ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: DAEMON_VERSION.to_string(),
            hostname: config.hostname.to_string(),
            session_id: state.session_id,
            allowed_actions: config.actions.allowed_kinds(),
            dry_run: config.actions.dry_run(),
        },
    )
    .await;

    send_message(
        &mut writer,
        &ServerMessage::RecentAnomalies {
            anomalies: state
                .recent_anomalies()
                .iter()
                .map(|event| (**event).clone())
                .collect(),
        },
    )
    .await;

    if subscription.snapshots {
        let snapshot = state.latest_snapshot();
        send_message(&mut writer, &ServerMessage::Snapshot((*snapshot).clone())).await;
    }

    run_session(reader, writer, state, shutdown, &config, subscription).await;
}

/// Wartet auf ein gültiges `Hello` innerhalb von `config.hello_timeout`.
/// Andere Nachrichten davor führen zu `Error(HelloExpected)`, die
/// Verbindung bleibt bis zum Ablauf der Frist offen (Regel 1).
async fn do_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    config: &ClientTaskConfig,
) -> HandshakeOutcome
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let deadline = Instant::now() + config.hello_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            send_goodbye(writer, GoodbyeReason::HelloTimeout).await;
            return HandshakeOutcome::Closed;
        }

        let frame =
            tokio::time::timeout(remaining, read_frame(reader, MAX_CLIENT_LINE_BYTES)).await;
        let line = match frame {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => return HandshakeOutcome::Closed,
            Ok(Err(FrameError::TooLong { .. })) => {
                send_goodbye(writer, GoodbyeReason::LineTooLong).await;
                return HandshakeOutcome::Closed;
            }
            Ok(Err(_)) => {
                send_error(
                    writer,
                    ErrorCode::MalformedMessage,
                    "ungültiges UTF-8",
                    None,
                )
                .await;
                return HandshakeOutcome::Closed;
            }
            Err(_elapsed) => {
                send_goodbye(writer, GoodbyeReason::HelloTimeout).await;
                return HandshakeOutcome::Closed;
            }
        };

        match parse_client_message(&line) {
            ParsedLine::Message(ClientMessage::Hello {
                protocol_version,
                subscription,
                ..
            }) => {
                if protocol_version != PROTOCOL_VERSION {
                    send_goodbye(
                        writer,
                        GoodbyeReason::VersionMismatch {
                            expected: PROTOCOL_VERSION,
                            got: protocol_version,
                        },
                    )
                    .await;
                    return HandshakeOutcome::Closed;
                }
                return HandshakeOutcome::Ready(subscription);
            }
            ParsedLine::Message(_) => {
                send_error(
                    writer,
                    ErrorCode::HelloExpected,
                    "erwarte Hello als erste Nachricht",
                    None,
                )
                .await;
            }
            ParsedLine::UnknownMessage => {
                send_error(
                    writer,
                    ErrorCode::UnknownMessage,
                    "unbekannter Nachrichtentyp vor Hello",
                    None,
                )
                .await;
            }
            ParsedLine::MalformedJson => {
                send_error(writer, ErrorCode::MalformedMessage, "ungültiges JSON", None).await;
                return HandshakeOutcome::Closed;
            }
        }
    }
}

/// Baut ein `tokio::time::Interval` für den Snapshot-Push, das
/// clientseitig gewünschte Intervall auf
/// `[min_snapshot_interval_ms, MAX_SNAPSHOT_INTERVAL_MS]` geklemmt.
fn make_snapshot_interval(
    subscription: &Subscription,
    config: &ClientTaskConfig,
) -> tokio::time::Interval {
    let clamped = subscription
        .snapshot_interval_ms
        .clamp(config.min_snapshot_interval_ms, MAX_SNAPSHOT_INTERVAL_MS)
        .max(1);
    let period = Duration::from_millis(u64::from(clamped));
    // `interval_at` statt `interval`: Letzteres feuert den ersten Tick
    // sofort, aber `run()` hat den ersten Snapshot bereits explizit
    // gesendet (Abschnitt 4: "sofort, danach im Abo-Intervall") -- ein
    // sofortiger zusätzlicher Tick hier würde ihn verdoppeln.
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Hauptschleife nach erfolgreichem Handshake: Snapshot-Push im
/// Abo-Intervall, Anomalie-Push per Broadcast, eingehende
/// Client-Nachrichten, Shutdown.
async fn run_session<R, W>(
    mut reader: R,
    mut writer: W,
    state: Arc<SharedState>,
    mut shutdown: watch::Receiver<bool>,
    config: &ClientTaskConfig,
    mut subscription: Subscription,
) where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut anomaly_rx = state.subscribe_anomalies();
    let mut snapshot_interval = make_snapshot_interval(&subscription, config);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    send_goodbye(&mut writer, GoodbyeReason::Shutdown).await;
                    return;
                }
            }
            _ = snapshot_interval.tick(), if subscription.snapshots => {
                let snapshot = state.latest_snapshot();
                send_message(&mut writer, &ServerMessage::Snapshot((*snapshot).clone())).await;
            }
            received = anomaly_rx.recv() => {
                match received {
                    Ok(event) => {
                        if subscription.anomalies {
                            send_message(&mut writer, &ServerMessage::Anomaly((*event).clone())).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        send_error(
                            &mut writer,
                            ErrorCode::Lagged { missed },
                            "Anomalien wurden verpasst, der Client hinkt hinterher",
                            None,
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            frame = read_frame(&mut reader, MAX_CLIENT_LINE_BYTES) => {
                match frame {
                    Ok(Some(line)) => match parse_client_message(&line) {
                        ParsedLine::Message(msg) => {
                            handle_client_message(msg, &mut writer, &state, config, &mut subscription).await;
                            snapshot_interval = make_snapshot_interval(&subscription, config);
                        }
                        ParsedLine::UnknownMessage => {
                            send_error(&mut writer, ErrorCode::UnknownMessage, "unbekannter Nachrichtentyp", None).await;
                        }
                        ParsedLine::MalformedJson => {
                            send_error(&mut writer, ErrorCode::MalformedMessage, "ungültiges JSON", None).await;
                            return;
                        }
                    },
                    Ok(None) => return,
                    Err(FrameError::TooLong { .. }) => {
                        send_goodbye(&mut writer, GoodbyeReason::LineTooLong).await;
                        return;
                    }
                    Err(_) => return,
                }
            }
        }
    }
}

/// Verarbeitet eine einzelne Nachricht in der laufenden Session
/// (nach dem Handshake).
async fn handle_client_message<W>(
    msg: ClientMessage,
    writer: &mut W,
    state: &Arc<SharedState>,
    config: &ClientTaskConfig,
    subscription: &mut Subscription,
) where
    W: AsyncWrite + Unpin,
{
    match msg {
        // Ein zweites Hello (z. B. durch einen fehlerhaften Client) wird
        // stillschweigend ignoriert -- die Verbindung ist bereits etabliert.
        ClientMessage::Hello { .. } => {}
        ClientMessage::Subscribe {
            subscription: requested,
        } => {
            let clamped_interval = requested
                .snapshot_interval_ms
                .clamp(config.min_snapshot_interval_ms, MAX_SNAPSHOT_INTERVAL_MS);
            *subscription = Subscription {
                snapshot_interval_ms: clamped_interval,
                ..requested
            };
        }
        ClientMessage::GetContext {
            request_id,
            timestamp_us,
            before,
            after,
            unit,
        } => {
            let before = before.min(config.context_max_lines);
            let after = after.min(config.context_max_lines);
            let (lines, truncated) =
                state.query_context(timestamp_us, before, after, unit.as_deref());
            let reply = ContextReply {
                request_id,
                lines: lines.iter().map(ContextLine::from).collect(),
                truncated,
            };
            send_message(writer, &ServerMessage::Context(reply)).await;
        }
        ClientMessage::Action { request_id, action } => {
            let outcome = config
                .actions
                .execute(&action, request_id, state.session_id, now_us(), state)
                .await;
            send_message(
                writer,
                &ServerMessage::ActionResult { request_id, outcome },
            )
            .await;
        }
        ClientMessage::Ping { nonce } => {
            send_message(writer, &ServerMessage::Pong { nonce }).await;
        }
    }
}

async fn send_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &ServerMessage) {
    match serde_json::to_string(message) {
        Ok(line) => {
            if let Err(err) = write_frame(writer, &line).await {
                tracing::debug!(fehler = %err, "Schreiben an Client fehlgeschlagen, Verbindung vermutlich tot");
            }
        }
        Err(err) => {
            tracing::warn!(fehler = %err, "Nachricht konnte nicht serialisiert werden");
        }
    }
}

async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: ErrorCode,
    message: &str,
    request_id: Option<u64>,
) {
    send_message(
        writer,
        &ServerMessage::Error {
            code,
            message: message.to_string(),
            request_id,
        },
    )
    .await;
}

async fn send_goodbye<W: AsyncWrite + Unpin>(writer: &mut W, reason: GoodbyeReason) {
    send_message(writer, &ServerMessage::Goodbye { reason }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use logsentry_proto::{
        ActionOutcome, AnomalyEvent, AnomalyLevel, DenyReason, LearningState, PipelineStats,
        RateSource, Snapshot, WindowStats,
    };
    use std::path::Path;

    use tokio::io::{AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::net::{UnixListener, UnixStream};

    fn minimal_snapshot() -> Snapshot {
        Snapshot {
            timestamp_us: 0,
            daemon_uptime_secs: 0,
            learning: LearningState {
                active: false,
                remaining_secs: None,
            },
            stats: PipelineStats {
                events: 0,
                parse_errors: 0,
                dropped_overflow: 0,
                templates: 0,
                anomalies_emitted: 0,
                suppressed: 0,
                suppressed_learning: 0,
                events_per_sec: 0.0,
                clients: 0,
            },
            window: WindowStats {
                window_secs: 60,
                entropy_bits: 0.0,
                entropy_z: 0.0,
                events_in_window: 0,
                distinct_templates: 0,
            },
            system: None,
            replay: false,
        }
    }

    fn sample_anomaly_event(id: u64) -> AnomalyEvent {
        AnomalyEvent {
            id,
            timestamp_us: id,
            template_id: 1,
            template_text: "x".into(),
            sample_message: "x".into(),
            unit: None,
            pid: None,
            priority: None,
            level: AnomalyLevel::Warn,
            breakdown: logsentry_proto::ScoreBreakdown {
                rate_z: 0.0,
                surprisal_bits: 0.0,
                entropy_z: 0.0,
                rate_component: 0.0,
                surprisal_component: 0.0,
                entropy_component: 0.0,
                combined: 0.0,
                rate_source: RateSource::None,
            },
            suppressed_since_last: 0,
        }
    }

    struct TestConn {
        reader: TokioBufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl TestConn {
        async fn connect(path: &Path) -> Self {
            let stream = UnixStream::connect(path).await.unwrap();
            let (r, w) = stream.into_split();
            Self {
                reader: TokioBufReader::new(r),
                writer: w,
            }
        }

        async fn send(&mut self, msg: &ClientMessage) {
            let line = serde_json::to_string(msg).unwrap();
            write_frame(&mut self.writer, &line).await.unwrap();
        }

        async fn send_raw(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn recv(&mut self) -> Option<ServerMessage> {
            let line = read_frame(&mut self.reader, MAX_CLIENT_LINE_BYTES)
                .await
                .unwrap()?;
            Some(serde_json::from_str(&line).unwrap())
        }

        async fn recv_timeout(&mut self) -> Option<ServerMessage> {
            tokio::time::timeout(Duration::from_secs(2), self.recv())
                .await
                .expect("Antwort erwartet, aber Timeout")
        }
    }

    fn default_hello(subscription: Subscription) -> ClientMessage {
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test-client".to_string(),
            client_version: "0.0.0".to_string(),
            subscription,
        }
    }

    async fn spawn_server(
        state: Arc<SharedState>,
        max_clients: u32,
    ) -> (std::path::PathBuf, tempfile::TempDir, watch::Sender<bool>) {
        let actions = Arc::new(ActionExecutor::new(logsentry_core::config::ActionsConfig::default()));
        spawn_server_with_actions(state, max_clients, actions).await
    }

    async fn spawn_server_with_actions(
        state: Arc<SharedState>,
        max_clients: u32,
        actions: Arc<ActionExecutor>,
    ) -> (std::path::PathBuf, tempfile::TempDir, watch::Sender<bool>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collector.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let hostname: Arc<str> = Arc::from("test-host");

        let config = logsentry_core::config::SocketConfig {
            path: path.to_string_lossy().to_string(),
            group: String::new(),
            mode: 0o660,
            max_clients,
            snapshot_interval_ms: 1000,
            min_snapshot_interval_ms: 50,
            hello_timeout_ms: 500,
            recent_anomalies: 50,
            context_lines: 50,
            context_max_lines: 50,
        };

        tokio::spawn(async move {
            crate::server::run(listener, config, hostname, state, actions, shutdown_rx).await;
        });

        (path, dir, shutdown_tx)
    }

    #[tokio::test]
    async fn handshake_liefert_hello_recent_anomalies_und_snapshot() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(Arc::clone(&state), 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Hello {
                protocol_version,
                session_id,
                ..
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(session_id, state.session_id);
            }
            other => panic!("Hello erwartet, bekam {other:?}"),
        }

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::RecentAnomalies { anomalies } => assert!(anomalies.is_empty()),
            other => panic!("RecentAnomalies erwartet, bekam {other:?}"),
        }

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Snapshot(_) => {}
            other => panic!("Snapshot erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn version_mismatch_fuehrt_zu_goodbye_und_verbindungsende() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            client_name: "test".to_string(),
            client_version: "0".to_string(),
            subscription: Subscription::default(),
        })
        .await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Goodbye {
                reason: GoodbyeReason::VersionMismatch { expected, got },
            } => {
                assert_eq!(expected, PROTOCOL_VERSION);
                assert_eq!(got, PROTOCOL_VERSION + 1);
            }
            other => panic!("Goodbye(VersionMismatch) erwartet, bekam {other:?}"),
        }

        assert!(
            conn.recv_timeout().await.is_none(),
            "Verbindung sollte danach schließen"
        );
    }

    #[tokio::test]
    async fn ueberlange_zeile_fuehrt_zu_goodbye_und_close() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;
        let _ = conn.recv_timeout().await; // Hello
        let _ = conn.recv_timeout().await; // RecentAnomalies
        let _ = conn.recv_timeout().await; // Snapshot

        let too_long = "x".repeat(MAX_CLIENT_LINE_BYTES + 10);
        conn.send_raw(&too_long).await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Goodbye {
                reason: GoodbyeReason::LineTooLong,
            } => {}
            other => panic!("Goodbye(LineTooLong) erwartet, bekam {other:?}"),
        }
        assert!(conn.recv_timeout().await.is_none());
    }

    #[tokio::test]
    async fn zweiter_client_bei_max_clients_eins_bekommt_goodbye() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(Arc::clone(&state), 1).await;

        // Erster Client verbindet und bleibt bestehen -- er belegt den
        // einzigen erlaubten Platz.
        let mut first = TestConn::connect(&path).await;
        first.send(&default_hello(Subscription::default())).await;
        let _ = first.recv_timeout().await; // Hello
        let _ = first.recv_timeout().await; // RecentAnomalies
        let _ = first.recv_timeout().await; // Snapshot

        let mut second = TestConn::connect(&path).await;
        match second.recv_timeout().await.unwrap() {
            ServerMessage::Goodbye {
                reason: GoodbyeReason::TooManyClients { max },
            } => assert_eq!(max, 1),
            other => panic!("Goodbye(TooManyClients) erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn langsamer_client_bekommt_lagged_fehler() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 300, 10));
        let (path, _dir, _shutdown) = spawn_server(Arc::clone(&state), 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription {
            snapshots: false,
            anomalies: true,
            snapshot_interval_ms: 1000,
        }))
        .await;
        let _ = conn.recv_timeout().await; // Hello
        let _ = conn.recv_timeout().await; // RecentAnomalies

        // Mehr Anomalien veröffentlichen, als der Broadcast-Puffer fasst
        // (256), bevor der Client-Task Gelegenheit bekommt, sie
        // abzuholen. Der aktuelle Task läuft `#[tokio::test]` einfädig,
        // die Schleife hier enthält kein `.await` und blockiert den
        // Client-Task damit vollständig, bis die Flut vorbei ist.
        for i in 0..400u64 {
            state.publish_anomaly(sample_anomaly_event(i));
        }

        let mut saw_lagged = false;
        for _ in 0..500 {
            match conn.recv_timeout().await {
                Some(ServerMessage::Error {
                    code: ErrorCode::Lagged { missed },
                    ..
                }) if missed > 0 => {
                    saw_lagged = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(saw_lagged, "erwartete mindestens eine Lagged-Fehlermeldung");
    }

    #[test]
    fn parse_erkennt_gueltige_nachricht_unbekannten_typ_und_ungueltiges_json() {
        let valid = r#"{"type":"ping","nonce":1}"#;
        assert!(matches!(
            parse_client_message(valid),
            ParsedLine::Message(ClientMessage::Ping { nonce: 1 })
        ));

        let unknown = r#"{"type":"does_not_exist"}"#;
        assert!(matches!(
            parse_client_message(unknown),
            ParsedLine::UnknownMessage
        ));

        let malformed = "{nicht valides json";
        assert!(matches!(
            parse_client_message(malformed),
            ParsedLine::MalformedJson
        ));
    }

    #[tokio::test]
    async fn ping_wird_mit_pong_beantwortet() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;

        conn.send(&ClientMessage::Ping { nonce: 42 }).await;
        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Pong { nonce } => assert_eq!(nonce, 42),
            other => panic!("Pong erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn action_ohne_konfigurierte_allow_list_wird_abgelehnt() {
        // spawn_server() baut den ActionExecutor mit ActionsConfig::default()
        // (leere Allow-Liste, Regel 9) -- die Ablehnung kommt jetzt vom
        // echten Executor, nicht mehr von einer Phase-6-Pauschalablehnung.
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;

        conn.send(&ClientMessage::Action {
            request_id: 1,
            action: logsentry_proto::ActionRequest::MuteAnomaly {
                template_id: 1,
                unit: None,
                scope: logsentry_proto::MuteScope::OneHour,
            },
        })
        .await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::ActionResult {
                request_id,
                outcome:
                    ActionOutcome::Denied {
                        reason: DenyReason::NotAllowed,
                    },
            } => assert_eq!(request_id, 1),
            other => panic!("ActionResult(Denied) erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn erlaubte_action_wird_ausgefuehrt_und_wirkt_im_shared_state() {
        // MuteAnomaly hat als einzige Aktionsart keinen externen
        // Seiteneffekt (kein D-Bus, kein Signal, kein Subprozess) --
        // anders als bei RestartUnit/TerminateProcess/BlockIp ist ein
        // echter (nicht Dry-Run-) Testlauf hier unbedenklich (Regel 26
        // schützt das reale System, nicht den eigenen Prozessspeicher).
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let actions_config = logsentry_core::config::ActionsConfig {
            dry_run: false,
            allowed_kinds: vec!["mute_anomaly".to_string()],
            ..logsentry_core::config::ActionsConfig::default()
        };
        let actions = Arc::new(ActionExecutor::new(actions_config));
        let (path, _dir, _shutdown) =
            spawn_server_with_actions(Arc::clone(&state), 8, actions).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;

        conn.send(&ClientMessage::Action {
            request_id: 7,
            action: logsentry_proto::ActionRequest::MuteAnomaly {
                template_id: 42,
                unit: None,
                scope: logsentry_proto::MuteScope::Permanent,
            },
        })
        .await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::ActionResult {
                request_id,
                outcome: ActionOutcome::Completed { dry_run, .. },
            } => {
                assert_eq!(request_id, 7);
                assert!(!dry_run);
            }
            other => panic!("ActionResult(Completed) erwartet, bekam {other:?}"),
        }

        assert!(state.is_muted(42, None, 0));
    }

    #[tokio::test]
    async fn get_context_liefert_umgebung_um_zeitstempel() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 50));
        for i in 0..5u64 {
            state.push_context(crate::wire::ContextEntry {
                timestamp_us: i * 10,
                unit: None,
                pid: Some(1),
                priority: Some(6),
                message: Arc::from(format!("zeile-{i}")),
            });
        }
        let (path, _dir, _shutdown) = spawn_server(Arc::clone(&state), 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&default_hello(Subscription::default())).await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;
        let _ = conn.recv_timeout().await;

        conn.send(&ClientMessage::GetContext {
            request_id: 7,
            timestamp_us: 20,
            before: 1,
            after: 1,
            unit: None,
        })
        .await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Context(reply) => {
                assert_eq!(reply.request_id, 7);
                assert_eq!(reply.lines.len(), 3);
                assert_eq!(reply.lines[1].message, "zeile-2");
                assert!(!reply.truncated);
            }
            other => panic!("Context erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn nachricht_vor_hello_bekommt_error_und_verbindung_bleibt_offen() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        conn.send(&ClientMessage::Ping { nonce: 1 }).await;

        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Error {
                code: ErrorCode::HelloExpected,
                ..
            } => {}
            other => panic!("Error(HelloExpected) erwartet, bekam {other:?}"),
        }

        // Verbindung ist noch offen: ein korrektes Hello danach klappt.
        conn.send(&default_hello(Subscription::default())).await;
        match conn.recv_timeout().await.unwrap() {
            ServerMessage::Hello { .. } => {}
            other => panic!("Hello erwartet, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn ohne_hello_innerhalb_der_frist_gibt_es_hello_timeout() {
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        let (path, _dir, _shutdown) = spawn_server(state, 8).await;

        let mut conn = TestConn::connect(&path).await;
        // Nichts senden, nur auf die Handshake-Frist (500 ms in
        // `spawn_server`) warten.
        match tokio::time::timeout(Duration::from_secs(2), conn.recv())
            .await
            .expect("Goodbye(HelloTimeout) erwartet")
        {
            Some(ServerMessage::Goodbye {
                reason: GoodbyeReason::HelloTimeout,
            }) => {}
            other => panic!("Goodbye(HelloTimeout) erwartet, bekam {other:?}"),
        }
    }
}
