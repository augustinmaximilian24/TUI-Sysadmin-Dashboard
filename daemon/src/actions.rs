//! Aktions-Subsystem: Allow-List- und Rate-Limit-Prüfung, Dispatch je
//! Aktionsart, Audit-Log.
//!
//! Normativ: `docs/phase8-aktionen.md`. Dieser Schritt (3 der dortigen
//! Umsetzungsreihenfolge) prüft Allow-List und Rate-Limit bereits
//! vollständig und schreibt das Audit-Log, führt aber noch keine echte
//! Aktion aus -- jede erlaubte Anfrage bekommt `Completed{dry_run: true}`.
//! Die echte Ausführung je Aktionsart folgt in den Schritten 4-7.
//!
//! `#![allow(dead_code)]`: `client_task.rs` ruft `ActionExecutor` erst ab
//! Schritt 8 auf (`docs/phase8-aktionen.md` Abschnitt 8); bis dahin ist
//! dieses Modul über seine eigenen Tests abgedeckt. Wird entfernt, sobald
//! Schritt 8 abgeschlossen ist.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Mutex;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde::Serialize;
use zbus::Connection;

use logsentry_core::config::ActionsConfig;
use logsentry_proto::{ActionOutcome, ActionRequest, DenyReason, MuteScope};

use crate::state::SharedState;
use crate::units::SystemdManagerProxy;

/// Führt Aktionsanfragen aus: Allow-List, Rate-Limit, Audit-Log, Dispatch.
///
/// Eine Instanz pro Daemon-Lauf, geteilt über `Arc` mit allen
/// Client-Verbindungen (analog zu `SharedState`) -- das Rate-Limit muss
/// über Verbindungen hinweg gelten, nicht pro Client zurückgesetzt werden.
pub struct ActionExecutor {
    config: ActionsConfig,
    rate_limits: Mutex<HashMap<String, VecDeque<u64>>>,
    audit_log: Mutex<Option<std::fs::File>>,
}

/// Eine Zeile im Audit-Log (Regel 13): ein Eintrag pro *Versuch*, auch
/// abgelehnte oder fehlgeschlagene, damit das Rate-Limit nicht durch
/// absichtliches Scheitern umgangen werden kann und die Audit-Ansicht
/// (Phase 8, GUI-Schritt) ein vollständiges Bild zeigt.
#[derive(Serialize)]
struct AuditEntry<'a> {
    timestamp_us: u64,
    session_id: u64,
    request_id: u64,
    kind: &'static str,
    target: &'a str,
    outcome: &'a ActionOutcome,
}

/// Bündelt die Herkunft eines Aktionsversuchs für [`ActionExecutor::audit`]
/// -- reines Zusammenfassen von Aufrufkontext, keine eigene Logik.
struct AuditContext<'a> {
    request_id: u64,
    session_id: u64,
    kind: &'static str,
    target: &'a str,
}

impl ActionExecutor {
    /// Baut den Executor. Ein nicht öffnenbares Audit-Log (z. B. fehlende
    /// Rechte im Entwicklungsbetrieb ohne Root) ist kein Startabbruch --
    /// Aktionen laufen dann ohne Audit-Trail weiter, mit einer einmaligen
    /// Warnung, analog zum Umgang mit einer nicht nutzbaren
    /// Baseline-Datei in `main.rs`.
    pub fn new(config: ActionsConfig) -> Self {
        let audit_log = open_audit_log(&config.audit_log_path);
        Self {
            config,
            rate_limits: Mutex::new(HashMap::new()),
            audit_log: Mutex::new(audit_log),
        }
    }

    /// Prüft Allow-List und Rate-Limit und führt die Aktion aus (Schritt 3:
    /// noch als Dry-Run-Platzhalter). `request_id`/`session_id` dienen nur
    /// dem Audit-Log, nicht der fachlichen Prüfung.
    pub async fn execute(
        &self,
        action: &ActionRequest,
        request_id: u64,
        session_id: u64,
        now_us: u64,
        state: &SharedState,
    ) -> ActionOutcome {
        let kind = action_kind_str(action);
        let target = action_target(action);

        let ctx = AuditContext {
            request_id,
            session_id,
            kind,
            target: &target,
        };

        if !self.config.allowed_kinds.iter().any(|k| k == kind) {
            let outcome = ActionOutcome::Denied {
                reason: DenyReason::NotAllowed,
            };
            self.audit(ctx, &outcome, now_us);
            return outcome;
        }

        if let Some(unit) = restart_or_stop_unit_name(action) {
            if !self.config.allowed_units.iter().any(|u| u == unit) {
                let outcome = ActionOutcome::Denied {
                    reason: DenyReason::NotAllowed,
                };
                self.audit(ctx, &outcome, now_us);
                return outcome;
            }
        }

        let rate_key = format!("{kind}:{target}");
        if let Some(retry_after_secs) = self.check_rate_limit(&rate_key, now_us) {
            let outcome = ActionOutcome::Denied {
                reason: DenyReason::RateLimited { retry_after_secs },
            };
            self.audit(ctx, &outcome, now_us);
            return outcome;
        }

        let outcome = match action {
            ActionRequest::RestartUnit { unit } => {
                self.dispatch_unit_action(unit.as_str(), false, now_us, state).await
            }
            ActionRequest::StopUnit { unit } => {
                self.dispatch_unit_action(unit.as_str(), true, now_us, state).await
            }
            ActionRequest::TerminateProcess { pid, grace_secs } => {
                self.dispatch_terminate_process(*pid, *grace_secs, now_us, state)
                    .await
            }
            ActionRequest::BlockIp { ip, duration_secs } => {
                self.dispatch_block_ip(*ip, *duration_secs).await
            }
            ActionRequest::MuteAnomaly {
                template_id,
                unit,
                scope,
            } => {
                self.dispatch_mute_anomaly(*template_id, unit.clone(), *scope, now_us, state)
            }
        };
        self.audit(ctx, &outcome, now_us);
        outcome
    }

    /// Führt `RestartUnit`/`StopUnit` aus (Schritt 4): im globalen Dry-Run
    /// nur eine Bestätigung ohne D-Bus-Aufruf, sonst echter Aufruf über
    /// `org.freedesktop.systemd1.Manager` mit anschließendem
    /// Selbstfilter-Eintrag für die Ziel-Unit (Regel 13).
    async fn dispatch_unit_action(
        &self,
        unit: &str,
        stop: bool,
        now_us: u64,
        state: &SharedState,
    ) -> ActionOutcome {
        let kind = if stop { "stop_unit" } else { "restart_unit" };

        if self.config.dry_run {
            return ActionOutcome::Completed {
                message: format!("Dry-Run: {kind} für {unit} würde jetzt ausgeführt"),
                dry_run: true,
            };
        }

        match restart_or_stop_unit_via_dbus(unit, stop).await {
            Ok(()) => {
                let until_us = now_us.saturating_add(
                    self.config.self_filter_window_secs.saturating_mul(1_000_000),
                );
                state.suppress_unit(unit, until_us);
                ActionOutcome::Completed {
                    message: format!("{kind} für {unit} ausgeführt"),
                    dry_run: false,
                }
            }
            Err(message) => ActionOutcome::Failed { message },
        }
    }

    /// Führt `TerminateProcess` aus (Schritt 5): SIGTERM sofort, danach ein
    /// entkoppelter Hintergrund-Task, der nach der (geklemmten) Gnadenfrist
    /// per Existenzprüfung entscheidet, ob SIGKILL nötig ist. Der
    /// `ActionOutcome` bezieht sich nur auf das SIGTERM -- das Protokoll
    /// hat für die Eskalation keinen eigenen Rückmeldeweg, ein zweites
    /// `ActionResult` für dieselbe `request_id` wäre nicht vorgesehen.
    async fn dispatch_terminate_process(
        &self,
        pid: u32,
        grace_secs: u16,
        now_us: u64,
        state: &SharedState,
    ) -> ActionOutcome {
        let grace = grace_secs.clamp(1, self.config.terminate_max_grace_secs);

        if self.config.dry_run {
            return ActionOutcome::Completed {
                message: format!(
                    "Dry-Run: terminate_process für PID {pid} (Grace {grace}s) würde jetzt ausgeführt"
                ),
                dry_run: true,
            };
        }

        let nix_pid = Pid::from_raw(pid as i32);
        match kill(nix_pid, Signal::SIGTERM) {
            Ok(()) => {
                let until_us = now_us.saturating_add(
                    self.config.self_filter_window_secs.saturating_mul(1_000_000),
                );
                state.suppress_pid(pid as i32, until_us);
                spawn_kill_escalation(nix_pid, grace);
                ActionOutcome::Completed {
                    message: format!(
                        "SIGTERM an PID {pid} gesendet, SIGKILL nach {grace}s falls nötig"
                    ),
                    dry_run: false,
                }
            }
            Err(errno) => ActionOutcome::Failed {
                message: format!("SIGTERM an PID {pid} fehlgeschlagen: {errno}"),
            },
        }
    }

    /// Führt `BlockIp` aus (Schritt 6): befristeter Eintrag in ein
    /// nftables-Set über den `nft`-Binärnamen, mit einzeln übergebenen
    /// Argumenten (nie über eine Shell, siehe Modul-Dokumentation und
    /// `docs/phase8-aktionen.md` Abschnitt 1). Kein Selbstfilter nötig --
    /// eine IP-Sperre erzeugt keine Journal-Zeilen der beobachteten Units.
    async fn dispatch_block_ip(&self, ip: std::net::IpAddr, duration_secs: u32) -> ActionOutcome {
        let duration = duration_secs.clamp(
            self.config.block_ip_min_duration_secs,
            self.config.block_ip_max_duration_secs,
        );

        if self.config.dry_run {
            return ActionOutcome::Completed {
                message: format!("Dry-Run: block_ip für {ip} ({duration}s) würde jetzt ausgeführt"),
                dry_run: true,
            };
        }

        match block_ip_via_nft(&self.config, ip, duration).await {
            Ok(()) => ActionOutcome::Completed {
                message: format!("{ip} für {duration}s gesperrt"),
                dry_run: false,
            },
            Err(message) => ActionOutcome::Failed { message },
        }
    }

    /// Führt `MuteAnomaly` aus (Schritt 7): reiner Zustandseintrag im
    /// `SharedState`-Mute-Speicher, kein externer Seiteneffekt -- deshalb
    /// synchron statt `async`. `unit: None` mutet das Template über alle
    /// Units hinweg (siehe `SharedState::mute`).
    fn dispatch_mute_anomaly(
        &self,
        template_id: u64,
        unit: Option<String>,
        scope: MuteScope,
        now_us: u64,
        state: &SharedState,
    ) -> ActionOutcome {
        let target = unit
            .as_deref()
            .map_or_else(|| template_id.to_string(), |u| format!("{template_id}@{u}"));

        if self.config.dry_run {
            return ActionOutcome::Completed {
                message: format!("Dry-Run: mute_anomaly für {target} würde jetzt eingetragen"),
                dry_run: true,
            };
        }

        let until_us = match scope {
            MuteScope::OneHour => now_us.saturating_add(3_600 * 1_000_000),
            MuteScope::OneDay => now_us.saturating_add(86_400 * 1_000_000),
            MuteScope::Permanent => u64::MAX,
        };
        state.mute(template_id, unit, until_us);
        ActionOutcome::Completed {
            message: format!("{target} stummgeschaltet ({scope:?})"),
            dry_run: false,
        }
    }

    /// Prüft und aktualisiert das Rate-Limit für `key`. `Some(retry_after)`
    /// bei Überschreitung, sonst `None` und der Versuch zählt (Regel: auch
    /// ein späterer `Denied` zählt, damit das Limit nicht durch
    /// absichtliches Scheitern umgangen werden kann -- deshalb zählt jeder
    /// Aufruf hier, unabhängig vom späteren Ausgang).
    fn check_rate_limit(&self, key: &str, now_us: u64) -> Option<u32> {
        let window_us = self
            .config
            .rate_limit_window_minutes
            .saturating_mul(60)
            .saturating_mul(1_000_000);
        let mut limits = self.rate_limits.lock().unwrap_or_else(|p| p.into_inner());
        let attempts = limits.entry(key.to_string()).or_default();
        attempts.retain(|&t| now_us.saturating_sub(t) < window_us);

        if attempts.len() as u32 >= self.config.rate_limit_max_actions {
            let oldest = attempts.front().copied().unwrap_or(now_us);
            let elapsed = now_us.saturating_sub(oldest);
            let retry_after_us = window_us.saturating_sub(elapsed.min(window_us));
            return Some((retry_after_us / 1_000_000) as u32);
        }

        attempts.push_back(now_us);
        None
    }

    fn audit(&self, ctx: AuditContext<'_>, outcome: &ActionOutcome, timestamp_us: u64) {
        let entry = AuditEntry {
            timestamp_us,
            session_id: ctx.session_id,
            request_id: ctx.request_id,
            kind: ctx.kind,
            target: ctx.target,
            outcome,
        };
        let Ok(mut line) = serde_json::to_string(&entry) else {
            tracing::warn!(kind = entry.kind, target = entry.target, "Audit-Eintrag konnte nicht serialisiert werden");
            return;
        };
        line.push('\n');

        let mut guard = self.audit_log.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(file) = guard.as_mut() {
            if let Err(err) = file.write_all(line.as_bytes()) {
                tracing::warn!(fehler = %err, "Audit-Log-Schreibvorgang fehlgeschlagen");
            }
        }
    }
}

/// Wartet `grace_secs`, prüft dann per Signal 0 (kein echtes Signal, nur
/// Fehlercode) ob der Prozess noch existiert, und sendet in diesem Fall
/// SIGKILL. Läuft entkoppelt vom aufrufenden Request -- der Client hat
/// sein `ActionResult` für das SIGTERM bereits erhalten.
fn spawn_kill_escalation(pid: Pid, grace_secs: u16) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(u64::from(grace_secs))).await;
        if kill(pid, None).is_ok() {
            match kill(pid, Signal::SIGKILL) {
                Ok(()) => tracing::info!(pid = pid.as_raw(), "SIGKILL nach Ablauf der Gnadenfrist gesendet"),
                Err(err) => tracing::warn!(pid = pid.as_raw(), fehler = %err, "SIGKILL nach Gnadenfrist fehlgeschlagen"),
            }
        }
    });
}

/// Sperrt `ip` für `duration_secs` über ein nftables-Set. `nft add table`
/// und `nft add set` sind von Haus aus idempotent (anders als `nft create
/// ...`, das bei bereits vorhandenem Objekt einen Fehler liefert) --
/// deshalb genügt es, Tabelle und Set bei jedem Aufruf "anzulegen", ohne
/// vorher zu prüfen, ob sie schon existieren.
async fn block_ip_via_nft(
    config: &ActionsConfig,
    ip: std::net::IpAddr,
    duration_secs: u32,
) -> Result<(), String> {
    let (set_name, set_type): (&str, &str) = match ip {
        std::net::IpAddr::V4(_) => (&config.nftables_set_v4, "ipv4_addr"),
        std::net::IpAddr::V6(_) => (&config.nftables_set_v6, "ipv6_addr"),
    };

    run_nft(&["add", "table", &config.nftables_family, &config.nftables_table]).await?;
    run_nft(&[
        "add",
        "set",
        &config.nftables_family,
        &config.nftables_table,
        set_name,
        "{",
        "type",
        set_type,
        ";",
        "flags",
        "timeout",
        ";",
        "}",
    ])
    .await?;
    let timeout_arg = format!("timeout {duration_secs}s");
    run_nft(&[
        "add",
        "element",
        &config.nftables_family,
        &config.nftables_table,
        set_name,
        "{",
        &ip.to_string(),
        &timeout_arg,
        "}",
    ])
    .await
}

/// Führt `nft` mit einzeln übergebenen Argumenten aus (kein `sh -c`, keine
/// String-Interpolation in eine Shell hinein -- Regel 8 gilt sinngemäß
/// auch für vom Daemon selbst konstruierte Kommandos). Ein nicht-Null-
/// Exitcode liefert `stderr` als Fehlertext, damit `ActionOutcome::Failed`
/// nie eine erfundene Erklärung zeigt.
async fn run_nft(args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("nft")
        .args(args)
        .output()
        .await
        .map_err(|err| format!("nft konnte nicht gestartet werden: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "nft {} fehlgeschlagen: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

/// Öffnet die Audit-Datei im Anhänge-Modus, legt das Elternverzeichnis bei
/// Bedarf an. `None` bei jedem Fehler -- der Aufrufer läuft dann ohne
/// Audit-Trail weiter statt den Daemon-Start zu verhindern.
fn open_audit_log(path: &str) -> Option<std::fs::File> {
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(pfad = %parent.display(), fehler = %err, "Audit-Verzeichnis konnte nicht angelegt werden, Aktionen laufen ohne Audit-Log");
                return None;
            }
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!(pfad = %path.display(), fehler = %err, "Audit-Log konnte nicht geöffnet werden, Aktionen laufen ohne Audit-Log");
            None
        }
    }
}

/// Kurzname der Aktionsart für Allow-List-Abgleich, Rate-Limit-Schlüssel
/// und Audit-Log (siehe `docs/phase8-aktionen.md` Abschnitt 3 für die in
/// der Konfiguration erwarteten Werte).
fn action_kind_str(action: &ActionRequest) -> &'static str {
    match action {
        ActionRequest::RestartUnit { .. } => "restart_unit",
        ActionRequest::StopUnit { .. } => "stop_unit",
        ActionRequest::TerminateProcess { .. } => "terminate_process",
        ActionRequest::BlockIp { .. } => "block_ip",
        ActionRequest::MuteAnomaly { .. } => "mute_anomaly",
    }
}

/// Menschlich lesbares Ziel für Rate-Limit-Schlüssel und Audit-Log.
fn action_target(action: &ActionRequest) -> String {
    match action {
        ActionRequest::RestartUnit { unit } | ActionRequest::StopUnit { unit } => {
            unit.as_str().to_string()
        }
        ActionRequest::TerminateProcess { pid, .. } => pid.to_string(),
        ActionRequest::BlockIp { ip, .. } => ip.to_string(),
        ActionRequest::MuteAnomaly {
            template_id, unit, ..
        } => match unit {
            Some(u) => format!("{template_id}@{u}"),
            None => template_id.to_string(),
        },
    }
}

/// Ruft `RestartUnit`/`StopUnit` über den System-D-Bus auf (Regel 10:
/// nicht `Command::new("systemctl")`). Verbindungsaufbau je Aufruf statt
/// einer gehaltenen Verbindung -- Aktionen sind selten genug, dass das
/// nicht ins Gewicht fällt, und ein Fehlschlag hier betrifft nicht den
/// unabhängigen `UnitMonitor` aus Phase 5.
async fn restart_or_stop_unit_via_dbus(unit: &str, stop: bool) -> Result<(), String> {
    let connection = Connection::system().await.map_err(|err| err.to_string())?;
    let manager = SystemdManagerProxy::new(&connection)
        .await
        .map_err(|err| err.to_string())?;
    let result = if stop {
        manager.stop_unit(unit, "replace").await
    } else {
        manager.restart_unit(unit, "replace").await
    };
    result.map(|_job_path| ()).map_err(|err| err.to_string())
}

/// Der Unit-Name, falls `action` eine `RestartUnit`/`StopUnit`-Anfrage ist
/// -- für den zusätzlichen Abgleich gegen `allowed_units`.
fn restart_or_stop_unit_name(action: &ActionRequest) -> Option<&str> {
    match action {
        ActionRequest::RestartUnit { unit } | ActionRequest::StopUnit { unit } => {
            Some(unit.as_str())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logsentry_proto::{MuteScope, UnitName};

    fn config(allowed_kinds: &[&str], allowed_units: &[&str]) -> ActionsConfig {
        ActionsConfig {
            allowed_kinds: allowed_kinds.iter().map(|s| s.to_string()).collect(),
            allowed_units: allowed_units.iter().map(|s| s.to_string()).collect(),
            audit_log_path: String::new(),
            ..ActionsConfig::default()
        }
    }

    fn test_state() -> SharedState {
        SharedState::new(
            logsentry_proto::Snapshot {
                timestamp_us: 0,
                daemon_uptime_secs: 0,
                learning: logsentry_proto::LearningState {
                    active: false,
                    remaining_secs: None,
                },
                stats: logsentry_proto::PipelineStats {
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
                window: logsentry_proto::WindowStats {
                    window_secs: 60,
                    entropy_bits: 0.0,
                    entropy_z: 0.0,
                    events_in_window: 0,
                    distinct_templates: 0,
                },
                system: None,
                replay: false,
            },
            10,
            10,
        )
    }

    fn restart(unit: &str) -> ActionRequest {
        ActionRequest::RestartUnit {
            unit: UnitName::parse(unit).unwrap(),
        }
    }

    #[tokio::test]
    async fn block_ip_bleibt_im_dry_run_ohne_echten_nft_aufruf() {
        // Regel 26: kein automatisierter Test verändert die echte
        // nftables-Konfiguration dieser Maschine.
        let state = test_state();
        let executor = ActionExecutor::new(config(&["block_ip"], &[]));
        let action = ActionRequest::BlockIp {
            ip: "203.0.113.5".parse().unwrap(),
            duration_secs: 300,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        match outcome {
            ActionOutcome::Completed { dry_run, message } => {
                assert!(dry_run);
                assert!(message.contains("Dry-Run"));
                assert!(message.contains("300s"));
            }
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_ip_ohne_allow_list_wird_abgelehnt() {
        let state = test_state();
        let executor = ActionExecutor::new(config(&[], &[]));
        let action = ActionRequest::BlockIp {
            ip: "203.0.113.5".parse().unwrap(),
            duration_secs: 300,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        assert_eq!(
            outcome,
            ActionOutcome::Denied {
                reason: DenyReason::NotAllowed
            }
        );
    }

    #[tokio::test]
    async fn block_ip_klemmt_dauer_auf_konfigurierte_grenzen() {
        let state = test_state();
        let mut cfg = config(&["block_ip"], &[]);
        cfg.block_ip_min_duration_secs = 60;
        cfg.block_ip_max_duration_secs = 120;
        let executor = ActionExecutor::new(cfg);
        let action = ActionRequest::BlockIp {
            ip: "203.0.113.5".parse().unwrap(),
            duration_secs: 999_999,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        match outcome {
            ActionOutcome::Completed { message, .. } => {
                assert!(message.contains("120s"), "Nachricht: {message}");
            }
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminate_process_bleibt_im_dry_run_ohne_echtes_signal() {
        // Regel 26: automatisierte Tests senden nie ein echtes Signal.
        // Default-Konfiguration ist dry_run=true, PID 1 (init/systemd)
        // würde bei einem echten SIGTERM ohnehin sofort mit
        // "Operation not permitted" scheitern -- hier zeigt gerade das
        // Ausbleiben eines solchen Fehlers, dass kein Signal gesendet wurde.
        let state = test_state();
        let executor = ActionExecutor::new(config(&["terminate_process"], &[]));
        let action = ActionRequest::TerminateProcess {
            pid: 1,
            grace_secs: 5,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        match outcome {
            ActionOutcome::Completed { dry_run, message } => {
                assert!(dry_run);
                assert!(message.contains("Dry-Run"));
            }
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminate_process_klemmt_grace_secs_auf_das_konfigurierte_maximum() {
        let state = test_state();
        let mut cfg = config(&["terminate_process"], &[]);
        cfg.terminate_max_grace_secs = 10;
        let executor = ActionExecutor::new(cfg);
        let action = ActionRequest::TerminateProcess {
            pid: 1,
            grace_secs: 999,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        match outcome {
            ActionOutcome::Completed { message, .. } => {
                assert!(message.contains("Grace 10s"), "Nachricht: {message}");
            }
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminate_process_ohne_allow_list_wird_abgelehnt() {
        let state = test_state();
        let executor = ActionExecutor::new(config(&[], &[]));
        let action = ActionRequest::TerminateProcess {
            pid: 1,
            grace_secs: 5,
        };
        let outcome = executor.execute(&action, 1, 1, 0, &state).await;
        assert_eq!(
            outcome,
            ActionOutcome::Denied {
                reason: DenyReason::NotAllowed
            }
        );
    }

    #[tokio::test]
    async fn unbekannte_aktionsart_wird_abgelehnt() {
        let state = test_state();
        let executor = ActionExecutor::new(config(&[], &[]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0, &state)
            .await;
        assert_eq!(
            outcome,
            ActionOutcome::Denied {
                reason: DenyReason::NotAllowed
            }
        );
    }

    #[tokio::test]
    async fn erlaubte_aktionsart_aber_unit_nicht_in_allow_list_wird_abgelehnt() {
        let state = test_state();
        let executor = ActionExecutor::new(config(&["restart_unit"], &["cron.service"]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0, &state)
            .await;
        assert_eq!(
            outcome,
            ActionOutcome::Denied {
                reason: DenyReason::NotAllowed
            }
        );
    }

    #[tokio::test]
    async fn erlaubte_aktion_mit_erlaubter_unit_wird_als_dry_run_bestaetigt() {
        // Default-Konfiguration ist bewusst dry_run=true (Regel 26: Tests
        // laufen nur im Dry-Run) -- dieser Test ruft also nie den echten
        // D-Bus auf.
        let state = test_state();
        let executor = ActionExecutor::new(config(&["restart_unit"], &["sshd.service"]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0, &state)
            .await;
        match outcome {
            ActionOutcome::Completed { dry_run, .. } => assert!(dry_run),
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn vierter_versuch_innerhalb_des_fensters_wird_rate_limitiert() {
        let state = test_state();
        let mut cfg = config(&["restart_unit"], &["sshd.service"]);
        cfg.rate_limit_max_actions = 3;
        cfg.rate_limit_window_minutes = 10;
        let executor = ActionExecutor::new(cfg);
        let one_minute_us = 60 * 1_000_000;

        for i in 0..3 {
            let outcome = executor
                .execute(&restart("sshd.service"), i, 1, i * one_minute_us, &state)
                .await;
            assert!(matches!(outcome, ActionOutcome::Completed { .. }));
        }

        let outcome = executor
            .execute(&restart("sshd.service"), 3, 1, 3 * one_minute_us, &state)
            .await;
        assert!(matches!(
            outcome,
            ActionOutcome::Denied {
                reason: DenyReason::RateLimited { .. }
            }
        ));
    }

    #[tokio::test]
    async fn rate_limit_gilt_pro_ziel_nicht_global() {
        let state = test_state();
        let mut cfg = config(&["restart_unit"], &["sshd.service", "cron.service"]);
        cfg.rate_limit_max_actions = 1;
        let executor = ActionExecutor::new(cfg);

        let first = executor
            .execute(&restart("sshd.service"), 1, 1, 0, &state)
            .await;
        let second = executor
            .execute(&restart("cron.service"), 2, 1, 0, &state)
            .await;
        assert!(matches!(first, ActionOutcome::Completed { .. }));
        assert!(matches!(second, ActionOutcome::Completed { .. }));
    }

    #[tokio::test]
    async fn abgelehnte_versuche_zaehlen_ebenfalls_fuers_rate_limit() {
        // Regression: ohne dieses Verhalten könnte ein Client das Limit
        // umgehen, indem er absichtlich eine zunächst nicht erlaubte
        // Variante schickt -- hier bleibt die Aktionsart von Anfang an
        // erlaubt, nur eine andere, ebenfalls gezählte Aktionsart wird
        // zwischengeschoben, um zu zeigen, dass derselbe Schlüssel über
        // mehrere `execute`-Aufrufe hinweg konsistent zählt.
        let state = test_state();
        let mut cfg = config(&["mute_anomaly"], &[]);
        cfg.rate_limit_max_actions = 1;
        let executor = ActionExecutor::new(cfg);

        let mute = ActionRequest::MuteAnomaly {
            template_id: 42,
            unit: None,
            scope: MuteScope::OneHour,
        };
        let first = executor.execute(&mute, 1, 1, 0, &state).await;
        let second = executor.execute(&mute, 2, 1, 0, &state).await;
        assert!(matches!(first, ActionOutcome::Completed { .. }));
        assert!(matches!(
            second,
            ActionOutcome::Denied {
                reason: DenyReason::RateLimited { .. }
            }
        ));
    }
}
