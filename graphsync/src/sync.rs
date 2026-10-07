//! Ablaufsteuerung: Durchsuchen -> Delta per Hash -> Extraktion ->
//! Ausgabe; daneben der begrenzte KI-Takt.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use logsentry_core::config::GraphSyncConfig;
use logsentry_core::hash::fnv1a_hash64;
use tracing::{info, warn};

use crate::extract::extract;
use crate::graph::{build, BuildParams};
use crate::llm::{build_prompt, parse_edges, validate, LlmRunner, PromptFile};
use crate::scan::{scan, ScanOptions};
use crate::state::{write_atomic, FileRecord, LlmBlocked, LoadOutcome, State};

/// Ersetzt ein führendes `~/` durch `home`.
pub fn expand_home(path: &str, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if path == "~" => home.map_or_else(|| PathBuf::from(path), PathBuf::from),
        _ => PathBuf::from(path),
    }
}

/// Ergebnis eines Prüflaufs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub files: usize,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// Größer als `max_file_bytes` oder nicht lesbar.
    pub skipped: usize,
    pub truncated: bool,
    pub unreadable_dirs: usize,
}

impl ScanReport {
    /// Ob sich der Bestand geändert hat.
    pub fn dirty(&self) -> bool {
        self.added + self.changed + self.removed > 0
    }
}

/// Ergebnis eines KI-Takts.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmTick {
    Disabled,
    NothingToDo,
    Blocked(LlmBlocked),
    Done {
        files: usize,
        edges: usize,
        rejected: usize,
        cost_usd: Option<f64>,
    },
    Failed(String),
}

/// Hält Konfiguration, Pfade und Zustand.
pub struct Syncer {
    config: GraphSyncConfig,
    roots: Vec<PathBuf>,
    state_path: PathBuf,
    output_path: PathBuf,
    pub state: State,
}

impl Syncer {
    /// Lädt den Zustand; Probleme beim Laden werden protokolliert, nie
    /// als Abbruch behandelt.
    pub fn new(config: GraphSyncConfig, home: Option<&str>) -> Self {
        let roots = config
            .source_dirs
            .iter()
            .map(|d| expand_home(d, home))
            .collect();
        let state_path = expand_home(&config.state_path, home);
        let output_path = expand_home(&config.output_path, home);
        let state = match State::load(&state_path) {
            LoadOutcome::Loaded(state) => state,
            LoadOutcome::Fresh(state) => {
                info!(path = %state_path.display(), "kein Zustand vorhanden, beginne neu");
                state
            }
            LoadOutcome::Reset {
                state,
                backup,
                reason,
            } => {
                warn!(%reason, %backup, "Zustand unbrauchbar, gesichert und neu begonnen");
                state
            }
        };
        Self {
            config,
            roots,
            state_path,
            output_path,
            state,
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Arbeitsverzeichnis für den KI-Prozess (leer, neben dem Zustand).
    pub fn llm_cwd(&self) -> PathBuf {
        self.state_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            .join("llm-cwd")
    }

    /// Durchsucht die Quellen und aktualisiert den Zustand. Gelesen und
    /// gehasht wird nur, wessen Größe oder mtime sich geändert hat.
    pub fn scan_once(&mut self, now: i64) -> ScanReport {
        let result = scan(
            &self.roots,
            &ScanOptions {
                extensions: &self.config.extensions,
                exclude_dir_names: &self.config.exclude_dir_names,
                skip_hidden_dirs: self.config.skip_hidden_dirs,
                max_files: self.config.max_files,
            },
        );
        let mut report = ScanReport {
            truncated: result.truncated,
            unreadable_dirs: result.unreadable_dirs,
            ..ScanReport::default()
        };
        let mut present: HashSet<String> = HashSet::new();
        for meta in result.files {
            let Some(key) = meta.path.to_str().map(str::to_string) else {
                report.skipped += 1;
                continue;
            };
            if meta.size > self.config.max_file_bytes {
                report.skipped += 1;
                continue;
            }
            if let Some(record) = self.state.files.get_mut(&key) {
                record.root = meta.root;
                if record.size == meta.size && record.mtime_ns == meta.mtime_ns {
                    present.insert(key);
                    continue;
                }
            }
            let Ok(bytes) = std::fs::read(&meta.path) else {
                report.skipped += 1;
                continue;
            };
            present.insert(key.clone());
            let hash = fnv1a_hash64(&bytes);
            if let Some(record) = self.state.files.get_mut(&key) {
                record.size = meta.size;
                record.mtime_ns = meta.mtime_ns;
                if record.hash == hash {
                    continue;
                }
            }
            // Nicht-UTF-8-Inhalt wird verlustbehaftet gelesen statt
            // verworfen (sinngemäß Regel 19).
            let content = String::from_utf8_lossy(&bytes);
            let extracted = extract(&content, self.config.min_term_len);
            let previous = self.state.files.remove(&key);
            if previous.is_some() {
                report.changed += 1;
            } else {
                report.added += 1;
            }
            let (llm_hash, llm_edges) = previous
                .map(|p| (p.llm_hash, p.llm_edges))
                .unwrap_or_default();
            self.state.files.insert(
                key,
                FileRecord {
                    root: meta.root,
                    size: meta.size,
                    mtime_ns: meta.mtime_ns,
                    hash,
                    changed_at: now,
                    title: extracted.title,
                    links: extracted.links,
                    terms: extracted.terms,
                    // Alte KI-Kanten bleiben bis zur nächsten Analyse
                    // sichtbar; `llm_hash` != `hash` markiert sie als fällig.
                    llm_hash,
                    llm_edges,
                },
            );
        }
        let before = self.state.files.len();
        self.state.files.retain(|path, _| present.contains(path));
        report.removed = before - self.state.files.len();
        report.files = self.state.files.len();
        report
    }

    /// Baut die Ausgabe und schreibt sie nur, wenn sie sich geändert hat
    /// (sonst würde die GUI unnötig neu layouten). Rückgabe: geschrieben?
    pub fn write_output(&self) -> io::Result<bool> {
        let graph = build(
            &self.state,
            &self.roots,
            &BuildParams {
                extensions: self.config.extensions.clone(),
                max_term_docs: self.config.max_term_docs,
                min_shared_score: self.config.min_shared_score,
                max_inferred_per_file: self.config.max_inferred_per_file,
                max_edges: self.config.max_edges,
            },
        );
        let json = serde_json::to_vec(&graph).map_err(io::Error::other)?;
        if std::fs::read(&self.output_path).is_ok_and(|old| old == json) {
            return Ok(false);
        }
        write_atomic(&self.output_path, &json)?;
        let extracted = graph
            .links
            .iter()
            .filter(|e| e.confidence == "EXTRACTED")
            .count();
        info!(
            nodes = graph.nodes.len(),
            extracted,
            inferred = graph.links.len() - extracted,
            path = %self.output_path.display(),
            "Verknüpfungen aktualisiert"
        );
        Ok(true)
    }

    pub fn save_state(&self) -> io::Result<()> {
        self.state.save(&self.state_path)
    }

    /// Dateien, die analysiert werden sollten: Inhalt seit der letzten
    /// Analyse geändert und seit `min_stable_secs` unverändert; neueste zuerst.
    pub fn llm_candidates(&self, now: i64) -> Vec<String> {
        let stable = i64::try_from(self.config.llm.min_stable_secs).unwrap_or(i64::MAX);
        let mut due: Vec<(&String, &FileRecord)> = self
            .state
            .files
            .iter()
            .filter(|(_, r)| r.llm_hash != Some(r.hash))
            .filter(|(_, r)| now.saturating_sub(r.changed_at) >= stable)
            .collect();
        due.sort_by(|a, b| b.1.changed_at.cmp(&a.1.changed_at).then(a.0.cmp(b.0)));
        due.into_iter()
            .take(self.config.llm.max_files_per_run)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Ein KI-Takt: prüft Budget und Wartezeit, analysiert höchstens einen
    /// Batch und verbucht das Ergebnis. `today` ist das lokale Datum.
    pub async fn llm_tick<R: LlmRunner>(&mut self, runner: &R, now: i64, today: &str) -> LlmTick {
        let llm = self.config.llm.clone();
        if !llm.enabled {
            return LlmTick::Disabled;
        }
        self.state.llm.roll_day(today);
        if let Err(blocked) = self
            .state
            .llm
            .check(now, llm.max_runs_per_day, llm.max_usd_per_day)
        {
            return LlmTick::Blocked(blocked);
        }
        let candidates = self.llm_candidates(now);
        if candidates.is_empty() {
            return LlmTick::NothingToDo;
        }

        // Aktuellen Inhalt lesen; hat er sich seit dem Scan geändert, wird
        // die Datei beim nächsten Takt berücksichtigt.
        let mut batch: Vec<(String, u64, String)> = Vec::new();
        for path in candidates {
            let Some(record) = self.state.files.get(&path) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if fnv1a_hash64(&bytes) == record.hash {
                batch.push((
                    path,
                    record.hash,
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }
        }
        if batch.is_empty() {
            return LlmTick::NothingToDo;
        }

        let (prompt_files, id_to_path) = self.prompt_files(&batch, llm.max_known_files_in_prompt);
        let path_to_id: HashMap<&String, &PromptFile> =
            prompt_files.iter().map(|(p, f)| (p, f)).collect();
        let batch_prompt: Vec<(PromptFile, String)> = batch
            .iter()
            .filter_map(|(path, _, content)| {
                path_to_id
                    .get(path)
                    .map(|f| ((*f).clone(), content.clone()))
            })
            .collect();
        let known: Vec<PromptFile> = prompt_files.iter().map(|(_, f)| f.clone()).collect();
        let batch_ids: HashSet<String> = batch_prompt.iter().map(|(f, _)| f.id.clone()).collect();
        let prompt = build_prompt(
            &batch_prompt,
            &known,
            llm.max_edges_per_run,
            llm.max_chars_per_file,
        );

        let result = runner.run(prompt).await.and_then(|reply| {
            let (raw, parse_rejected) = parse_edges(&reply.text)?;
            Ok((raw, parse_rejected, reply.cost_usd))
        });
        match result {
            Ok((raw, parse_rejected, cost_usd)) => {
                let (edges, rejected) =
                    validate(raw, &id_to_path, &batch_ids, llm.max_edges_per_run);
                let accepted: usize = edges.values().map(Vec::len).sum();
                for (path, hash, _) in &batch {
                    if let Some(record) = self.state.files.get_mut(path) {
                        record.llm_hash = Some(*hash);
                        record.llm_edges = edges.get(path).cloned().unwrap_or_default();
                    }
                }
                self.state
                    .llm
                    .record_success(now, cost_usd, llm.min_interval_secs);
                LlmTick::Done {
                    files: batch.len(),
                    edges: accepted,
                    rejected: rejected + parse_rejected,
                    cost_usd,
                }
            }
            Err(err) => {
                let message = err.to_string();
                self.state
                    .llm
                    .record_failure(now, &message, llm.failure_backoff_secs);
                LlmTick::Failed(message)
            }
        }
    }

    /// Vergibt Prompt-IDs: zuerst die Batch-Dateien, dann die übrigen nach
    /// letzter Änderung, insgesamt höchstens `max_known` (mindestens der Batch).
    fn prompt_files(
        &self,
        batch: &[(String, u64, String)],
        max_known: usize,
    ) -> (Vec<(String, PromptFile)>, HashMap<String, String>) {
        let in_batch: HashSet<&String> = batch.iter().map(|(p, _, _)| p).collect();
        let mut others: Vec<(&String, &FileRecord)> = self
            .state
            .files
            .iter()
            .filter(|(p, _)| !in_batch.contains(p))
            .collect();
        others.sort_by(|a, b| b.1.changed_at.cmp(&a.1.changed_at).then(a.0.cmp(b.0)));
        let ordered = batch
            .iter()
            .map(|(p, _, _)| p)
            .chain(others.into_iter().map(|(p, _)| p))
            .take(max_known.max(batch.len()));

        let mut files = Vec::new();
        let mut ids = HashMap::new();
        for (n, path) in ordered.enumerate() {
            let id = format!("F{}", n + 1);
            let title = self
                .state
                .files
                .get(path)
                .and_then(|r| r.title.clone())
                .unwrap_or_default();
            ids.insert(id.clone(), path.clone());
            files.push((
                path.clone(),
                PromptFile {
                    id,
                    display: self.display_path(path),
                    title,
                },
            ));
        }
        (files, ids)
    }

    /// Pfad relativ zur passenden Quellwurzel (kürzer im Prompt, verrät
    /// nicht unnötig die Verzeichnisstruktur außerhalb der Quellen).
    fn display_path(&self, path: &str) -> String {
        let p = Path::new(path);
        self.roots
            .iter()
            .filter_map(|root| {
                let rest = p.strip_prefix(root).ok()?;
                let name = root.file_name()?.to_string_lossy();
                Some(format!("{name}/{}", rest.display()))
            })
            .next()
            .unwrap_or_else(|| path.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmError, LlmReply};
    use std::future::Future;
    use std::sync::Mutex;

    /// Test-Runner: liefert eine feste Antwort und merkt sich den Prompt.
    struct FakeRunner {
        reply: Result<String, ()>,
        prompts: Mutex<Vec<String>>,
    }

    impl LlmRunner for FakeRunner {
        fn run(&self, prompt: String) -> impl Future<Output = Result<LlmReply, LlmError>> + Send {
            if let Ok(mut prompts) = self.prompts.lock() {
                prompts.push(prompt);
            }
            let reply = self.reply.clone();
            async move {
                match reply {
                    Ok(text) => Ok(LlmReply {
                        text,
                        cost_usd: Some(0.05),
                    }),
                    Err(()) => Err(LlmError::Timeout),
                }
            }
        }
    }

    fn config(root: &Path, state_dir: &Path) -> GraphSyncConfig {
        let mut config = GraphSyncConfig {
            source_dirs: vec![root.display().to_string()],
            state_path: state_dir.join("state.json").display().to_string(),
            output_path: state_dir.join("links.json").display().to_string(),
            ..GraphSyncConfig::default()
        };
        // Tests nutzen nur den FakeRunner, nie die echte CLI.
        config.llm.enabled = true;
        config.llm.min_stable_secs = 0;
        config.llm.min_interval_secs = 100;
        config.llm.failure_backoff_secs = 1000;
        config
    }

    #[test]
    fn delta_erkennt_neu_geaendert_entfernt_und_unveraendert() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(notes.path().join("a.md"), "[[b]]").expect("write");
        std::fs::write(notes.path().join("b.md"), "B").expect("write");
        let mut syncer = Syncer::new(config(notes.path(), state_dir.path()), None);

        let first = syncer.scan_once(10);
        assert_eq!((first.added, first.changed, first.removed), (2, 0, 0));
        assert!(
            !syncer.scan_once(11).dirty(),
            "nichts geändert -> kein Delta"
        );

        std::fs::write(notes.path().join("b.md"), "B geändert, länger").expect("write");
        std::fs::remove_file(notes.path().join("a.md")).expect("rm");
        let delta = syncer.scan_once(12);
        assert_eq!((delta.added, delta.changed, delta.removed), (0, 1, 1));
        assert_eq!(syncer.state.files.len(), 1);
    }

    #[test]
    fn ausgabe_wird_nur_bei_aenderung_geschrieben_und_zustand_ueberlebt_neustart() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(notes.path().join("a.md"), "[[b]]").expect("write");
        std::fs::write(notes.path().join("b.md"), "B").expect("write");
        let mut syncer = Syncer::new(config(notes.path(), state_dir.path()), None);
        syncer.scan_once(1);
        assert!(syncer.write_output().expect("write"));
        assert!(
            !syncer.write_output().expect("write"),
            "identisch -> nicht neu schreiben"
        );
        syncer.save_state().expect("save");

        let mut restarted = Syncer::new(config(notes.path(), state_dir.path()), None);
        assert!(
            !restarted.scan_once(2).dirty(),
            "Hashes aus dem Zustand -> kein Delta"
        );
        let out: serde_json::Value =
            serde_json::from_slice(&std::fs::read(restarted.output_path()).expect("read"))
                .expect("json");
        assert_eq!(out["links"][0]["confidence"], "EXTRACTED");
    }

    #[tokio::test]
    async fn ki_takt_uebernimmt_validierte_kanten_und_verbucht_kosten() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(notes.path().join("a.md"), "# A\nNAS-Backup").expect("write");
        std::fs::write(notes.path().join("b.md"), "# B\nNAS").expect("write");
        let mut syncer = Syncer::new(config(notes.path(), state_dir.path()), None);
        syncer.scan_once(0);

        // Beide Dateien im Batch; F1/F2 = a/b nach Änderungszeit, dann Pfad.
        let runner = FakeRunner {
            reply: Ok(r#"[{"source":"F1","target":"F2","relation":"Same Device"},{"source":"F1","target":"F99"}]"#.into()),
            prompts: Mutex::new(Vec::new()),
        };
        let tick = syncer.llm_tick(&runner, 10, "2026-10-07").await;
        assert_eq!(
            tick,
            LlmTick::Done {
                files: 2,
                edges: 1,
                rejected: 1,
                cost_usd: Some(0.05)
            }
        );
        let a = &syncer.state.files[&notes.path().join("a.md").display().to_string()];
        assert_eq!(a.llm_edges[0].relation, "same_device");
        assert_eq!(a.llm_hash, Some(a.hash));
        let prompt = runner.prompts.lock().expect("lock")[0].clone();
        assert!(prompt.contains("NAS-Backup"));

        // Alles analysiert und Mindestabstand aktiv.
        assert_eq!(
            syncer.llm_tick(&runner, 11, "2026-10-07").await,
            LlmTick::Blocked(LlmBlocked::Waiting)
        );
        assert_eq!(
            syncer.llm_tick(&runner, 500, "2026-10-07").await,
            LlmTick::NothingToDo
        );
        assert_eq!(syncer.state.llm.runs, 1);
        assert!((syncer.state.llm.usd - 0.05).abs() < 1e-9);
        assert!(syncer.write_output().expect("write"));
        let out = std::fs::read_to_string(syncer.output_path()).expect("read");
        assert!(out.contains("\"same_device\"") && out.contains("\"INFERRED\""));
    }

    #[tokio::test]
    async fn ki_fehler_markiert_nichts_und_wartet_backoff_ab() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(notes.path().join("a.md"), "A").expect("write");
        let mut syncer = Syncer::new(config(notes.path(), state_dir.path()), None);
        syncer.scan_once(0);
        let runner = FakeRunner {
            reply: Err(()),
            prompts: Mutex::new(Vec::new()),
        };
        assert!(matches!(
            syncer.llm_tick(&runner, 10, "d").await,
            LlmTick::Failed(_)
        ));
        assert!(syncer.state.files.values().all(|r| r.llm_hash.is_none()));
        assert_eq!(
            syncer.llm_tick(&runner, 500, "d").await,
            LlmTick::Blocked(LlmBlocked::Waiting)
        );
        assert!(syncer.state.llm.last_error.is_some());
    }

    #[tokio::test]
    async fn ki_wartet_bis_datei_stabil_ist_und_respektiert_tagesbudget() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(notes.path().join("a.md"), "A").expect("write");
        let mut cfg = config(notes.path(), state_dir.path());
        cfg.llm.min_stable_secs = 600;
        cfg.llm.max_usd_per_day = 0.05;
        cfg.llm.min_interval_secs = 0;
        let mut syncer = Syncer::new(cfg, None);
        syncer.scan_once(1000);
        let runner = FakeRunner {
            reply: Ok("[]".into()),
            prompts: Mutex::new(Vec::new()),
        };
        assert_eq!(
            syncer.llm_tick(&runner, 1100, "d").await,
            LlmTick::NothingToDo
        );
        assert!(matches!(
            syncer.llm_tick(&runner, 1600, "d").await,
            LlmTick::Done { .. }
        ));

        std::fs::write(notes.path().join("a.md"), "A2 länger").expect("write");
        syncer.scan_once(1700);
        assert_eq!(
            syncer.llm_tick(&runner, 9000, "d").await,
            LlmTick::Blocked(LlmBlocked::DailyBudget)
        );
        assert!(matches!(
            syncer.llm_tick(&runner, 9000, "neuer-tag").await,
            LlmTick::Done { .. }
        ));
    }

    #[tokio::test]
    async fn deaktivierte_ki_ruft_nichts_auf() {
        let notes = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = config(notes.path(), state_dir.path());
        cfg.llm.enabled = false;
        let mut syncer = Syncer::new(cfg, None);
        let runner = FakeRunner {
            reply: Ok("[]".into()),
            prompts: Mutex::new(Vec::new()),
        };
        assert_eq!(syncer.llm_tick(&runner, 0, "d").await, LlmTick::Disabled);
        assert!(runner.prompts.lock().expect("lock").is_empty());
    }

    #[test]
    fn expand_home_varianten() {
        assert_eq!(expand_home("~/x", Some("/h")), PathBuf::from("/h/x"));
        assert_eq!(expand_home("~", Some("/h")), PathBuf::from("/h"));
        assert_eq!(expand_home("/abs", Some("/h")), PathBuf::from("/abs"));
        assert_eq!(expand_home("~/x", None), PathBuf::from("~/x"));
    }
}
