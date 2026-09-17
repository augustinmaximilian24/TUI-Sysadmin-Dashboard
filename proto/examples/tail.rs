//! Minimaler Referenz-Client: verbindet sich zu einem `logsentry`-Daemon
//! und druckt jede eingehende Nachricht als eine Zeile JSON auf stdout.
//!
//! Dient Regel 28 (kein Feature gilt ohne Replay-Verifikation als fertig)
//! als Werkzeug, um im Replay-Modus zu beobachten, was der Daemon
//! tatsächlich über den Socket schickt, sowie `docs/phase6-protokoll.md`
//! Abschnitt 8 Schritt 6 als manueller Rauchtest der Pipeline-Anbindung.
//! Verbindungsstatus geht auf stderr, damit `tail | jq` auf stdout sauber
//! bleibt.
//!
//! Aufruf: `cargo run -p logsentry-proto --features client --example tail -- /run/logsentry/collector.sock`

use std::path::PathBuf;

use logsentry_proto::{spawn, ClientConfig, ConnectionState, Subscription};

#[tokio::main]
async fn main() {
    let socket_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/logsentry/collector.sock"));

    eprintln!("verbinde zu {}", socket_path.display());

    let (_outbound, mut inbound, mut connection_state) = spawn(ClientConfig {
        socket_path,
        client_name: "logsentry-tail".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        subscription: Subscription::default(),
    });

    let mut state_watch = tokio::spawn(async move {
        let mut last = ConnectionState::Connecting { attempt: 0 };
        loop {
            if connection_state.changed().await.is_err() {
                return;
            }
            let current = connection_state.borrow().clone();
            if current != last {
                eprintln!("verbindungsstatus: {current:?}");
                last = current;
            }
        }
    });

    loop {
        tokio::select! {
            message = inbound.recv() => {
                match message {
                    Some(msg) => match serde_json::to_string(&msg) {
                        Ok(line) => println!("{line}"),
                        Err(err) => eprintln!("konnte Nachricht nicht serialisieren: {err}"),
                    },
                    None => {
                        eprintln!("Verbindungs-Task beendet");
                        break;
                    }
                }
            }
            _ = &mut state_watch => {
                eprintln!("Statusüberwachung beendet");
            }
        }
    }
}
