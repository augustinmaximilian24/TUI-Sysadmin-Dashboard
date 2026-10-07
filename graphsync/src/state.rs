//! Persistenter Zustand von `logsentry-graphsync`: pro Datei Hash,
//! extrahierte Links/Begriffe und KI-Ergebnis, dazu das KI-Tagesbudget.
//!
//! Bewusst eine JSON-Datei statt `redb`: der Zustand wird immer komplett
//! gelesen und geschrieben, ist durch `max_files` begrenzt und soll bei
//! Bedarf von Hand inspizierbar sein. Geschrieben wird atomar
//! (temporäre Datei + `rename`), damit ein Abbruch nie eine halbe Datei
//! hinterlässt.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::extract::LinkRef;

/// Aktuelle Schema-Version. Bei inkompatiblen Änderungen erhöhen und in
/// [`State::load`] einen Migrationsschritt ergänzen.
pub const SCHEMA_VERSION: u32 = 1;

/// Höchstzahl gespeicherter KI-Kanten pro Datei (Regel 18).
pub const MAX_LLM_EDGES_PER_FILE: usize = 30;

/// Gesamter Zustand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    /// Schlüssel: Dateipfad.
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    pub llm: LlmLedger,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            files: BTreeMap::new(),
            llm: LlmLedger::default(),
        }
    }
}

/// Zustand einer Datei.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Index der Quellwurzel (wird bei jedem Durchlauf aktualisiert).
    pub root: usize,
    pub size: u64,
    pub mtime_ns: i64,
    /// FNV-1a über den Dateiinhalt (Änderungserkennung, kein Schutz gegen
    /// absichtliche Kollisionen -- hier nicht nötig).
    pub hash: u64,
    /// Unix-Sekunden, zu denen sich der Inhalt zuletzt geändert hat.
    pub changed_at: i64,
    pub title: Option<String>,
    pub links: Vec<LinkRef>,
    pub terms: Vec<String>,
    /// Hash des Inhalts, der zuletzt von der KI analysiert wurde.
    #[serde(default)]
    pub llm_hash: Option<u64>,
    /// Von der KI gefundene Verknüpfungen dieser Datei.
    #[serde(default)]
    pub llm_edges: Vec<LlmEdge>,
}

/// Eine von der KI gefundene Verknüpfung (Ziel als Dateipfad).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmEdge {
    pub target: String,
    pub relation: String,
}

/// Verbrauch der KI-Stufe (Tagesbudget und Wartezeiten).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmLedger {
    /// Lokales Datum (`YYYY-MM-DD`), auf das sich `runs`/`usd` beziehen.
    pub day: String,
    pub runs: u32,
    pub usd: f64,
    /// Frühester Zeitpunkt (Unix-Sekunden) für den nächsten Aufruf.
    pub next_allowed_unix: i64,
    pub total_runs: u64,
    pub total_usd: f64,
    pub last_error: Option<String>,
}

/// Grund, warum gerade kein KI-Aufruf erlaubt ist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBlocked {
    DailyRuns,
    DailyBudget,
    Waiting,
}

impl LlmLedger {
    /// Setzt die Tageszähler zurück, wenn ein neuer Tag begonnen hat.
    pub fn roll_day(&mut self, today: &str) {
        if self.day != today {
            self.day = today.to_string();
            self.runs = 0;
            self.usd = 0.0;
        }
    }

    /// Prüft Tageslimits und Mindestabstand.
    pub fn check(&self, now: i64, max_runs: u32, max_usd: f64) -> Result<(), LlmBlocked> {
        if self.runs >= max_runs {
            Err(LlmBlocked::DailyRuns)
        } else if self.usd >= max_usd {
            Err(LlmBlocked::DailyBudget)
        } else if now < self.next_allowed_unix {
            Err(LlmBlocked::Waiting)
        } else {
            Ok(())
        }
    }

    /// Verbucht einen erfolgreichen Aufruf.
    pub fn record_success(&mut self, now: i64, cost_usd: Option<f64>, min_interval_secs: u64) {
        let cost = cost_usd
            .filter(|c| c.is_finite() && *c >= 0.0)
            .unwrap_or(0.0);
        self.runs += 1;
        self.total_runs += 1;
        self.usd += cost;
        self.total_usd += cost;
        self.next_allowed_unix = now.saturating_add(secs(min_interval_secs));
        self.last_error = None;
    }

    /// Verbucht einen fehlgeschlagenen Aufruf. Zählt mit, damit ein
    /// dauerhaft scheiternder Aufruf (z. B. ohne Netz) nicht endlos läuft.
    pub fn record_failure(&mut self, now: i64, error: &str, backoff_secs: u64) {
        self.runs += 1;
        self.total_runs += 1;
        self.next_allowed_unix = now.saturating_add(secs(backoff_secs));
        self.last_error = Some(error.chars().take(300).collect());
    }
}

fn secs(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Ergebnis von [`State::load`].
#[derive(Debug)]
pub enum LoadOutcome {
    Loaded(State),
    /// Datei fehlt -- erster Start.
    Fresh(State),
    /// Datei war unlesbar oder von einer neueren Version; sie wurde unter
    /// dem angegebenen Pfad gesichert und es wird neu begonnen.
    Reset {
        state: State,
        backup: String,
        reason: String,
    },
}

impl State {
    /// Lädt den Zustand. Fehler führen nie zum Abbruch: eine unbrauchbare
    /// Datei wird gesichert und durch einen leeren Zustand ersetzt (die
    /// Offline-Stufen bauen alles beim nächsten Durchlauf neu auf).
    pub fn load(path: &Path) -> LoadOutcome {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return LoadOutcome::Fresh(Self::default());
            }
            Err(err) => return reset(path, &format!("nicht lesbar: {err}")),
        };
        match serde_json::from_str::<State>(&raw) {
            Ok(state) if state.schema_version == SCHEMA_VERSION => LoadOutcome::Loaded(state),
            Ok(state) => reset(
                path,
                &format!(
                    "Schema-Version {} wird nicht unterstützt (erwartet {SCHEMA_VERSION})",
                    state.schema_version
                ),
            ),
            Err(err) => reset(path, &format!("nicht parsebar: {err}")),
        }
    }

    /// Schreibt den Zustand atomar.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec(self).map_err(io::Error::other)?;
        write_atomic(path, &json)
    }
}

fn reset(path: &Path, reason: &str) -> LoadOutcome {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = format!("{}.bak-{stamp}", path.display());
    // Sicherung ist Best-Effort: scheitert sie, geht es trotzdem weiter.
    let _ = std::fs::rename(path, &backup);
    LoadOutcome::Reset {
        state: State::default(),
        backup,
        reason: reason.to_string(),
    }
}

/// Schreibt `data` über eine temporäre Datei im selben Verzeichnis und
/// benennt sie dann um; legt fehlende Elternverzeichnisse an.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> FileRecord {
        FileRecord {
            root: 0,
            size: 3,
            mtime_ns: 1,
            hash: 42,
            changed_at: 100,
            title: Some("T".into()),
            links: vec![LinkRef::Wiki("x".into())],
            terms: vec!["#a".into()],
            llm_hash: None,
            llm_edges: vec![],
        }
    }

    #[test]
    fn speichern_und_laden_ergibt_denselben_zustand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/state.json");
        let mut state = State::default();
        state.files.insert("/n/a.md".into(), record());
        state.llm.runs = 2;
        state.save(&path).expect("save");
        match State::load(&path) {
            LoadOutcome::Loaded(loaded) => assert_eq!(loaded, state),
            other => panic!("erwartet Loaded, war {other:?}"),
        }
    }

    #[test]
    fn fehlende_datei_ist_erster_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            State::load(&dir.path().join("x.json")),
            LoadOutcome::Fresh(_)
        ));
    }

    #[test]
    fn kaputte_oder_neuere_datei_wird_gesichert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{kaputt").expect("write");
        let LoadOutcome::Reset { backup, .. } = State::load(&path) else {
            panic!("erwartet Reset");
        };
        assert!(Path::new(&backup).exists());
        assert!(!path.exists());

        std::fs::write(&path, r#"{"schema_version": 99}"#).expect("write");
        assert!(matches!(State::load(&path), LoadOutcome::Reset { .. }));
    }

    #[test]
    fn tagesbudget_und_wartezeiten() {
        let mut ledger = LlmLedger::default();
        ledger.roll_day("2026-10-07");
        assert_eq!(ledger.check(0, 2, 1.0), Ok(()));

        ledger.record_success(100, Some(0.4), 60);
        assert_eq!(ledger.check(120, 2, 1.0), Err(LlmBlocked::Waiting));
        assert_eq!(ledger.check(160, 2, 1.0), Ok(()));

        ledger.record_failure(160, "kein Netz", 600);
        assert_eq!(ledger.check(10_000, 2, 1.0), Err(LlmBlocked::DailyRuns));
        assert_eq!(ledger.last_error.as_deref(), Some("kein Netz"));

        ledger.roll_day("2026-10-08");
        assert_eq!(ledger.runs, 0);
        ledger.usd = 1.0;
        assert_eq!(ledger.check(10_000, 2, 1.0), Err(LlmBlocked::DailyBudget));
        assert_eq!(ledger.total_runs, 2);
    }

    #[test]
    fn negative_oder_ungueltige_kosten_zaehlen_nicht() {
        let mut ledger = LlmLedger::default();
        ledger.record_success(0, Some(f64::NAN), 0);
        ledger.record_success(0, Some(-1.0), 0);
        assert_eq!(ledger.usd, 0.0);
    }
}
