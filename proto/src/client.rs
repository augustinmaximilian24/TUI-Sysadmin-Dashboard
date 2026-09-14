//! Client-seitiger Verbindungs-Task: verbindet zu einem Unix-Socket,
//! führt den Handshake aus, hält die Verbindung mit Backoff am Leben.
//!
//! Wird sowohl von der GUI (Phase 7) als auch vom künftigen TUI-Client
//! (Phase 10) genutzt -- deshalb hier zentral und nicht in `gui/`. Normativ:
//! `docs/phase6-protokoll.md` Abschnitt 4 (Verbindungsablauf) und
//! Abschnitt 6 (dieses Modul).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::framing::{read_frame, write_frame};
use crate::wire::{
    ClientMessage, GoodbyeReason, ServerMessage, Subscription, MAX_SERVER_LINE_BYTES,
    PROTOCOL_VERSION,
};

/// Startparameter für [`spawn`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub socket_path: PathBuf,
    pub client_name: String,
    pub client_version: String,
    pub subscription: Subscription,
}

/// Sichtbarer Verbindungsstatus für die UI (Kopfbereich, Regel: Phase 7).
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Connecting {
        attempt: u32,
    },
    Connected {
        session_id: u64,
    },
    /// Zugriff verweigert (z. B. Gruppenmitgliedschaft fehlt). Kein
    /// Reconnect-Sturm: Neuversuch erst nach `DENIED_RETRY` (Abschnitt 6).
    Denied(ClientError),
    Disconnected {
        retry_in: Duration,
    },
}

/// Fehlerbilder aus `docs/phase6-protokoll.md` Abschnitt 5, mit fertigem
/// Anzeigetext, damit GUI und TUI denselben Wortlaut zeigen.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ClientError {
    #[error("Daemon nicht erreichbar – `systemctl status logsentry`")]
    NotRunning,
    #[error(
        "Kein Zugriff auf den Socket. `sudo usermod -aG logsentry $USER`, danach neu anmelden."
    )]
    AccessDenied,
    #[error("Daemon abgestürzt? Socket-Datei verwaist – Daemon neu starten.")]
    Orphaned,
    #[error("Protokoll v{got} vs. v{expected} – Daemon und GUI gemeinsam aktualisieren.")]
    VersionMismatch { expected: u16, got: u16 },
    #[error("Verbindung zum Daemon getrennt: {0}")]
    Other(String),
}

impl ClientError {
    fn from_connect_io(err: &io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotRunning,
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::ConnectionRefused => Self::Orphaned,
            _ => Self::Other(err.to_string()),
        }
    }
}

/// Backoff-Parameter (Abschnitt 6): 500 ms · 2^n, Deckel 30 s, ±20 % Jitter.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Fester Neuversuchsabstand bei `EACCES`, um keinen Reconnect-Sturm gegen
/// einen absichtlich verweigerten Zugriff zu erzeugen.
const DENIED_RETRY: Duration = Duration::from_secs(30);
/// Größe der Receiver-Queue Richtung Anwendung (Abschnitt 6: bounded 128).
const INBOUND_CAPACITY: usize = 128;
/// Größe der Sender-Queue Richtung Daemon.
const OUTBOUND_CAPACITY: usize = 32;

fn backoff_delay(attempt: u32) -> Duration {
    let exp = attempt.min(6); // 500ms * 2^6 = 32s, danach ohnehin gekappt
    let raw = BACKOFF_BASE.saturating_mul(1u32 << exp);
    let capped = raw.min(BACKOFF_CAP);
    // Deterministischer Jitter ohne zusätzliche RNG-Abhängigkeit: Streuung
    // über die low bits der Systemzeit, Bereich [-20%, +20%].
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_permille = (nanos % 400) as i64 - 200; // -200..=199
    let base_ms = capped.as_millis() as i64;
    let jittered_ms = (base_ms + base_ms * jitter_permille / 1000).max(0);
    Duration::from_millis(jittered_ms as u64)
}

/// Startet den Verbindungs-Task. Liefert einen Sender für
/// Client→Daemon-Nachrichten, einen [`InboundReceiver`] für
/// Daemon→Client-Nachrichten sowie ein `watch` für den Verbindungsstatus
/// (Kopfbereich in Phase 7).
///
/// [`InboundReceiver`] ist bounded ([`INBOUND_CAPACITY`]). Läuft die Queue
/// voll, weil die Anwendung nicht abholt, wird der älteste **Snapshot**
/// verworfen (sichtbar über [`InboundReceiver::dropped_local`]) --
/// Snapshots sind ersetzbar. Gibt es keinen Snapshot in der Queue zu
/// verwerfen (nur Anomalien/Ergebnisse warten), wird stattdessen das
/// älteste Element verworfen, statt unbegrenzt zu wachsen (Regel 18) --
/// das ist der einzige Fall, in dem theoretisch auch eine Anomalie
/// verworfen werden kann, und wird ebenfalls gezählt.
pub fn spawn(
    cfg: ClientConfig,
) -> (
    mpsc::Sender<ClientMessage>,
    InboundReceiver,
    watch::Receiver<ConnectionState>,
) {
    let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let (inbound_tx, inbound_rx) = inbound_channel(INBOUND_CAPACITY);
    let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting { attempt: 0 });

    tokio::spawn(connection_loop(cfg, outbound_rx, inbound_tx, state_tx));

    (outbound_tx, inbound_rx, state_rx)
}

async fn connection_loop(
    cfg: ClientConfig,
    mut outbound_rx: mpsc::Receiver<ClientMessage>,
    inbound_tx: InboundSender,
    state_tx: watch::Sender<ConnectionState>,
) {
    // Bekannte Grenze: Lässt die Anwendung den outbound-Sender fallen,
    // während wir noch nie verbunden waren (Connecting/Backoff-Warten),
    // wird das erst nach dem nächsten erfolgreichen Handshake bemerkt --
    // `mpsc::Receiver` bietet kein `is_closed()`, um das vorher zu prüfen.
    // In der Praxis hält die GUI den Sender ohnehin für ihre gesamte
    // Laufzeit; für Phase 6 wird das bewusst nicht weiter behandelt.
    let mut attempt: u32 = 0;
    loop {
        let _ = state_tx.send(ConnectionState::Connecting { attempt });

        match connect_and_handshake(&cfg).await {
            Ok((session_id, stream)) => {
                attempt = 0;
                let _ = state_tx.send(ConnectionState::Connected { session_id });
                match run_session(stream, &mut outbound_rx, &inbound_tx).await {
                    SessionEnd::OutboundClosed => {
                        inbound_tx.close().await;
                        return;
                    }
                    SessionEnd::Lost => {
                        // Unten mit Backoff neu verbinden.
                    }
                }
            }
            Err(ClientError::AccessDenied) => {
                let _ = state_tx.send(ConnectionState::Denied(ClientError::AccessDenied));
                tokio::time::sleep(DENIED_RETRY).await;
                continue;
            }
            Err(_err) => {
                let delay = backoff_delay(attempt);
                let _ = state_tx.send(ConnectionState::Disconnected { retry_in: delay });
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                continue;
            }
        }

        let delay = backoff_delay(attempt);
        let _ = state_tx.send(ConnectionState::Disconnected { retry_in: delay });
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

async fn connect_and_handshake(cfg: &ClientConfig) -> Result<(u64, UnixStream), ClientError> {
    let stream = UnixStream::connect(&cfg.socket_path)
        .await
        .map_err(|e| ClientError::from_connect_io(&e))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: cfg.client_name.clone(),
        client_version: cfg.client_version.clone(),
        subscription: cfg.subscription,
    };
    let line = serde_json::to_string(&hello).expect("ClientMessage ist immer serialisierbar");
    write_frame(&mut write_half, &line)
        .await
        .map_err(|e| ClientError::Other(e.to_string()))?;

    let raw = read_frame(&mut reader, MAX_SERVER_LINE_BYTES)
        .await
        .map_err(|e| ClientError::Other(e.to_string()))?
        .ok_or_else(|| ClientError::Other("Verbindung vor Hello-Antwort geschlossen".into()))?;
    let msg: ServerMessage =
        serde_json::from_str(&raw).map_err(|e| ClientError::Other(e.to_string()))?;

    match msg {
        ServerMessage::Hello {
            protocol_version,
            session_id,
            ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(ClientError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    got: protocol_version,
                });
            }
            let stream = reader.into_inner().reunite(write_half).map_err(|_| {
                ClientError::Other(
                    "interner Fehler: Lese-/Schreibhälfte stammen nicht vom selben Stream".into(),
                )
            })?;
            Ok((session_id, stream))
        }
        ServerMessage::Goodbye {
            reason: GoodbyeReason::VersionMismatch { expected, got },
        } => Err(ClientError::VersionMismatch { expected, got }),
        ServerMessage::Goodbye { reason } => {
            Err(ClientError::Other(format!("Daemon lehnte ab: {reason:?}")))
        }
        other => Err(ClientError::Other(format!(
            "unerwartete erste Antwort vom Daemon: {other:?}"
        ))),
    }
}

/// Warum [`run_session`] zurückgekehrt ist.
enum SessionEnd {
    /// Die Anwendung hat den von [`spawn`] zurückgegebenen Sender fallen
    /// gelassen -- niemand will mehr etwas senden, der Task kann enden.
    OutboundClosed,
    /// Verbindung verloren (Daemon-Goodbye, EOF, I/O-/Framing-Fehler) --
    /// `connection_loop` soll erneut verbinden.
    Lost,
}

/// Verarbeitet eine etablierte Session, bis der Daemon trennt oder ein
/// I/O-Fehler auftritt. Weitergabe an die Anwendung über [`InboundSender`]
/// (Drop-Verhalten bei Überlauf siehe dort und an [`spawn`]).
async fn run_session(
    stream: UnixStream,
    outbound_rx: &mut mpsc::Receiver<ClientMessage>,
    inbound_tx: &InboundSender,
) -> SessionEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        tokio::select! {
            outgoing = outbound_rx.recv() => {
                let Some(msg) = outgoing else {
                    return SessionEnd::OutboundClosed;
                };
                let Ok(line) = serde_json::to_string(&msg) else {
                    continue;
                };
                if write_frame(&mut write_half, &line).await.is_err() {
                    return SessionEnd::Lost;
                }
            }
            frame = read_frame(&mut reader, MAX_SERVER_LINE_BYTES) => {
                match frame {
                    Ok(Some(raw)) => {
                        let Ok(msg) = serde_json::from_str::<ServerMessage>(&raw) else {
                            continue; // unbekannte/kaputte Nachricht ignorieren
                        };
                        let is_goodbye = matches!(msg, ServerMessage::Goodbye { .. });
                        inbound_tx.send(msg).await;
                        if is_goodbye {
                            return SessionEnd::Lost;
                        }
                    }
                    Ok(None) => return SessionEnd::Lost,  // sauberes EOF
                    Err(_) => return SessionEnd::Lost,    // Framing-/IO-Fehler
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Inbound-Queue: bounded, mit "ältesten Snapshot verwerfen"-Semantik bei
// Überlauf statt eines rohen `mpsc::Receiver`, der das strukturell nicht
// hergeben kann (ein `Sender` kann nicht aus der Queue lesen).
// ---------------------------------------------------------------------

struct InboundQueue {
    items: std::collections::VecDeque<ServerMessage>,
    capacity: usize,
    closed: bool,
}

/// Eingehender Nachrichten-Receiver für [`spawn`]. Siehe dortige
/// Dokumentation für das Verhalten bei voller Queue.
pub struct InboundReceiver {
    inner: std::sync::Arc<tokio::sync::Mutex<InboundQueue>>,
    notify: std::sync::Arc<tokio::sync::Notify>,
    dropped_local: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl InboundReceiver {
    /// Wartet auf die nächste Nachricht. `None`, sobald der Verbindungs-
    /// Task endgültig beendet ist (Anwendung hat `spawn`-Sender fallen
    /// gelassen); im normalen Betrieb liefert dieser Aufruf auch während
    /// eines Reconnects weiter, sobald wieder Daten da sind.
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        loop {
            {
                let mut q = self.inner.lock().await;
                if let Some(msg) = q.items.pop_front() {
                    return Some(msg);
                }
                if q.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// Anzahl lokal verworfener Nachrichten seit Verbindungsaufbau des
    /// Clients (nicht zu verwechseln mit dem serverseitigen
    /// `Lagged`-Zähler aus dem Broadcast-Kanal).
    pub fn dropped_local(&self) -> u64 {
        self.dropped_local
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct InboundSender {
    inner: std::sync::Arc<tokio::sync::Mutex<InboundQueue>>,
    notify: std::sync::Arc<tokio::sync::Notify>,
    dropped_local: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl InboundSender {
    async fn send(&self, msg: ServerMessage) {
        let mut q = self.inner.lock().await;
        if q.items.len() >= q.capacity {
            // Ältesten Snapshot verdrängen, falls einer wartet -- der ist
            // ersetzbar. Wartet keiner (nur Anomalien/Ergebnisse in der
            // Queue), muss trotzdem etwas weichen (Regel 18: keine
            // unbeschränkt wachsende Queue); dann das älteste Element,
            // unabhängig vom Typ.
            let victim = q
                .items
                .iter()
                .position(|m| matches!(m, ServerMessage::Snapshot(_)))
                .unwrap_or(0);
            q.items.remove(victim);
            self.dropped_local
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        q.items.push_back(msg);
        drop(q);
        self.notify.notify_waiters();
    }

    async fn close(&self) {
        let mut q = self.inner.lock().await;
        q.closed = true;
        drop(q);
        self.notify.notify_waiters();
    }
}

fn inbound_channel(capacity: usize) -> (InboundSender, InboundReceiver) {
    let inner = std::sync::Arc::new(tokio::sync::Mutex::new(InboundQueue {
        items: std::collections::VecDeque::with_capacity(capacity),
        capacity,
        closed: false,
    }));
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let dropped_local = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        InboundSender {
            inner: inner.clone(),
            notify: notify.clone(),
            dropped_local: dropped_local.clone(),
        },
        InboundReceiver {
            inner,
            notify,
            dropped_local,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    fn socket_path(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
        tmp.path().join(name)
    }

    fn test_config(path: PathBuf) -> ClientConfig {
        ClientConfig {
            socket_path: path,
            client_name: "logsentry-test".into(),
            client_version: "0.0.0".into(),
            subscription: Subscription::default(),
        }
    }

    /// Minimaler Mock-Server: akzeptiert eine Verbindung, liest das Hello,
    /// antwortet mit einem passenden Hello und hält die Verbindung offen.
    async fn accept_one_and_handshake(listener: &UnixListener, session_id: u64) -> UnixStream {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let _hello = read_frame(&mut reader, MAX_SERVER_LINE_BYTES)
            .await
            .unwrap()
            .unwrap();

        let reply = ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.0.0".into(),
            hostname: "test".into(),
            session_id,
            allowed_actions: vec![],
            dry_run: true,
        };
        write_frame(&mut write_half, &serde_json::to_string(&reply).unwrap())
            .await
            .unwrap();

        reader
            .into_inner()
            .reunite(write_half)
            .expect("Lese- und Schreibhälfte stammen aus demselben into_split()")
    }

    #[tokio::test]
    async fn verbindet_und_erreicht_connected_mit_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = socket_path(&tmp, "logsentry.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let stream = accept_one_and_handshake(&listener, 42).await;
            // Verbindung offen halten, bis der Test sie beendet.
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(stream);
        });

        let (_tx, _rx, mut state_rx) = spawn(test_config(path));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let ConnectionState::Connected { session_id } = &*state_rx.borrow() {
                assert_eq!(*session_id, 42);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("nicht innerhalb der Frist verbunden");
            }
            state_rx.changed().await.unwrap();
        }

        server.abort();
    }

    #[tokio::test]
    async fn fehlender_socket_fuehrt_zu_disconnected_und_backoff() {
        let tmp = tempfile::tempdir().unwrap();
        // Kein Listener gebunden -- ENOENT.
        let path = socket_path(&tmp, "nirgendwo.sock");

        let (_tx, _rx, mut state_rx) = spawn(test_config(path));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let ConnectionState::Disconnected { .. } = &*state_rx.borrow() {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("kein Disconnected-Status erreicht");
            }
            state_rx.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn reconnect_nach_serverseitigem_verbindungsabbruch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = socket_path(&tmp, "logsentry.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let (_tx, _rx, mut state_rx) = spawn(test_config(path));

        // Erste Verbindung annehmen und sofort wieder trennen.
        let (stream, _addr) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let _hello = read_frame(&mut reader, MAX_SERVER_LINE_BYTES)
            .await
            .unwrap()
            .unwrap();
        let stream = reader.into_inner();
        drop(stream);

        // Client muss auf Disconnected gehen und danach erneut verbinden.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(&*state_rx.borrow(), ConnectionState::Disconnected { .. }) {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("kein Disconnected nach Serverabbruch erreicht");
            }
            state_rx.changed().await.unwrap();
        }

        let (stream2, _addr2) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream2.into_split();
        let mut reader2 = BufReader::new(read_half);
        let _hello2 = read_frame(&mut reader2, MAX_SERVER_LINE_BYTES)
            .await
            .unwrap()
            .unwrap();
        let reply = ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.0.0".into(),
            hostname: "test".into(),
            session_id: 99,
            allowed_actions: vec![],
            dry_run: true,
        };
        write_frame(&mut write_half, &serde_json::to_string(&reply).unwrap())
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let ConnectionState::Connected { session_id } = &*state_rx.borrow() {
                assert_eq!(*session_id, 99);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("kein zweiter Connected-Status erreicht");
            }
            state_rx.changed().await.unwrap();
        }
    }

    #[test]
    fn backoff_waechst_und_wird_gekappt() {
        let d0 = backoff_delay(0);
        let d5 = backoff_delay(5);
        let d10 = backoff_delay(10);
        assert!(d0 >= Duration::from_millis(400) && d0 <= Duration::from_millis(600));
        assert!(d5 > d0);
        // Deckel 30s +/- 20% Jitter
        assert!(d10 <= Duration::from_millis(36_000));
    }
}
