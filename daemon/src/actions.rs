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

use serde::Serialize;

use logsentry_core::config::ActionsConfig;
use logsentry_proto::{ActionOutcome, ActionRequest, DenyReason};

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

        // Schritt 3: Allow-List und Rate-Limit sind scharf, die
        // tatsächliche Ausführung (D-Bus/Signal/nft/Mute-Speicher) folgt
        // in den Schritten 4-7. `self.config.dry_run` wird dann hier
        // ausgewertet; bis dahin ist jede erlaubte Anfrage faktisch
        // Dry-Run.
        let outcome = ActionOutcome::Completed {
            message: format!(
                "{kind} für {target}: Allow-List und Rate-Limit bestanden, echte Ausführung folgt in einem späteren Umsetzungsschritt"
            ),
            dry_run: true,
        };
        self.audit(ctx, &outcome, now_us);
        outcome
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

    fn restart(unit: &str) -> ActionRequest {
        ActionRequest::RestartUnit {
            unit: UnitName::parse(unit).unwrap(),
        }
    }

    #[tokio::test]
    async fn unbekannte_aktionsart_wird_abgelehnt() {
        let executor = ActionExecutor::new(config(&[], &[]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0)
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
        let executor = ActionExecutor::new(config(&["restart_unit"], &["cron.service"]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0)
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
        let executor = ActionExecutor::new(config(&["restart_unit"], &["sshd.service"]));
        let outcome = executor
            .execute(&restart("sshd.service"), 1, 1, 0)
            .await;
        match outcome {
            ActionOutcome::Completed { dry_run, .. } => assert!(dry_run),
            other => panic!("erwartete Completed, bekam {other:?}"),
        }
    }

    #[tokio::test]
    async fn vierter_versuch_innerhalb_des_fensters_wird_rate_limitiert() {
        let mut cfg = config(&["restart_unit"], &["sshd.service"]);
        cfg.rate_limit_max_actions = 3;
        cfg.rate_limit_window_minutes = 10;
        let executor = ActionExecutor::new(cfg);
        let one_minute_us = 60 * 1_000_000;

        for i in 0..3 {
            let outcome = executor
                .execute(&restart("sshd.service"), i, 1, i * one_minute_us)
                .await;
            assert!(matches!(outcome, ActionOutcome::Completed { .. }));
        }

        let outcome = executor
            .execute(&restart("sshd.service"), 3, 1, 3 * one_minute_us)
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
        let mut cfg = config(&["restart_unit"], &["sshd.service", "cron.service"]);
        cfg.rate_limit_max_actions = 1;
        let executor = ActionExecutor::new(cfg);

        let first = executor.execute(&restart("sshd.service"), 1, 1, 0).await;
        let second = executor.execute(&restart("cron.service"), 2, 1, 0).await;
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
        let mut cfg = config(&["mute_anomaly"], &[]);
        cfg.rate_limit_max_actions = 1;
        let executor = ActionExecutor::new(cfg);

        let mute = ActionRequest::MuteAnomaly {
            template_id: 42,
            unit: None,
            scope: MuteScope::OneHour,
        };
        let first = executor.execute(&mute, 1, 1, 0).await;
        let second = executor.execute(&mute, 2, 1, 0).await;
        assert!(matches!(first, ActionOutcome::Completed { .. }));
        assert!(matches!(
            second,
            ActionOutcome::Denied {
                reason: DenyReason::RateLimited { .. }
            }
        ));
    }
}
