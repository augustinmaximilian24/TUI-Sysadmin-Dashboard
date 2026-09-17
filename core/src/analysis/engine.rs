//! Zusammenführung der Einzelsignale zu einem Anomalie-Urteil.
//!
//! Drei Signale fließen ein:
//! - **Rate** (robuster Z-Score): Tritt dieses Template gerade viel häufiger
//!   auf als sonst?
//! - **Surprisal**: Wie unerwartet ist dieses Template überhaupt? Erstmals
//!   gesehene Templates schlagen hier aus.
//! - **Entropie**: Wie stark weicht die Vielfalt im Zeitfenster von der
//!   üblichen Vielfalt ab? Ein Log-Sturm einer einzelnen Quelle senkt die
//!   Entropie deutlich, ein plötzlicher Wildwuchs hebt sie.
//!
//! Jedes Signal wird auf 0..1 normiert und gedeckelt, danach über eine
//! **Noisy-OR-Verknüpfung** zusammengeführt:
//!
//! ```text
//! score = 1 - Π (1 - wᵢ·cᵢ)
//! ```
//!
//! Die Gewichte wᵢ sind dabei keine Anteile einer Mittelung, sondern die
//! maximale Durchschlagskraft des jeweiligen Signals. Eine gewichtete
//! *Mittelung* wäre hier strukturell falsch: Ein erstmals gesehenes Template
//! lässt nur das Surprisal ausschlagen, und dessen Gewichtsanteil an der
//! Summe deckelt den Gesamtscore auf einen Wert, der die Meldeschwelle nie
//! erreichen kann – die Anomalie wäre per Konstruktion unsichtbar. Die
//! Noisy-OR-Form drückt dagegen genau das gewünschte „irgendeines dieser
//! Anzeichen genügt" aus, bleibt monoton und liegt garantiert in 0..1.
//!
//! Drei Dämpfungsmechanismen verhindern Alarmfluten:
//! - **Lernphase**: In den ersten Minuten wird nur beobachtet.
//! - **Hysterese**: Ein Level wird erst wieder verlassen, wenn der Score
//!   spürbar unter die Auslöseschwelle fällt – nicht schon bei minimalem
//!   Unterschreiten.
//! - **Cooldown/Dedup**: Dasselbe Template meldet sich frühestens nach
//!   Ablauf einer Sperrzeit erneut.

use std::collections::HashMap;

use crate::baseline::{unit_key_from_name, BaselineStore, RateSource};
use crate::config::{AnalysisConfig, BaselineConfig};
use crate::template::TemplateId;

use super::rate::RateTracker;
use super::stats::{robust_z_score, surprisal, ENTROPY_MIN_SCALE};
use super::window::SlidingWindow;

/// Schweregrad einer erkannten Anomalie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnomalyLevel {
    /// Unauffällig – wird nicht gemeldet.
    #[default]
    Normal,
    /// Auffällig, aber unkritisch.
    Info,
    /// Deutliche Abweichung.
    Warn,
    /// Starke Abweichung.
    Critical,
}

impl AnomalyLevel {
    /// Kurzbezeichnung für Anzeige und Protokoll.
    pub fn as_str(&self) -> &'static str {
        match self {
            AnomalyLevel::Normal => "normal",
            AnomalyLevel::Info => "info",
            AnomalyLevel::Warn => "warn",
            AnomalyLevel::Critical => "critical",
        }
    }
}

/// Aufschlüsselung des Gesamtscores in seine Bestandteile.
///
/// Wird unverändert bis in das Detail-Panel der GUI durchgereicht (Phase 7),
/// damit nachvollziehbar bleibt, *warum* etwas gemeldet wurde.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreBreakdown {
    /// Robuster Z-Score der Ereignisrate dieses Templates.
    pub rate_z: f64,
    /// Surprisal des Templates in Bit.
    pub surprisal_bits: f64,
    /// Robuster Z-Score der Fenster-Entropie.
    pub entropy_z: f64,
    /// Normierter Rate-Anteil (0..1).
    pub rate_component: f64,
    /// Normierter Surprisal-Anteil (0..1).
    pub surprisal_component: f64,
    /// Normierter Entropie-Anteil (0..1).
    pub entropy_component: f64,
    /// Gewichteter Gesamtscore (0..1).
    pub combined: f64,
    /// Woher der Rate-Z-Score stammt (Fallback-Kette Slot-Baseline ->
    /// ANY-Slot-Baseline -> Kurzzeit-Historie, Phase 4).
    pub rate_source: RateSource,
}

/// Eine gemeldete Anomalie.
#[derive(Debug, Clone, PartialEq)]
pub struct Anomaly {
    /// Zeitpunkt des auslösenden Ereignisses (Mikrosekunden seit Epoch).
    pub timestamp_us: u64,
    /// Betroffenes Template.
    pub template_id: TemplateId,
    /// Betroffene systemd-Unit, sofern bekannt.
    pub unit: Option<String>,
    /// Ermittelter Schweregrad.
    pub level: AnomalyLevel,
    /// Aufschlüsselung des Scores.
    pub breakdown: ScoreBreakdown,
    /// Wie oft dieses Template seit der letzten Meldung unterdrückt wurde.
    pub suppressed_since_last: u64,
}

/// Eingabe für einen Analyseschritt.
#[derive(Debug, Clone)]
pub struct AnalysisInput<'a> {
    /// Zeitstempel des Ereignisses (Mikrosekunden seit Epoch).
    pub timestamp_us: u64,
    /// Template-Zuordnung des Ereignisses.
    pub template_id: TemplateId,
    /// Betroffene systemd-Unit, sofern bekannt.
    pub unit: Option<&'a str>,
}

/// Zustand pro Template für Hysterese und Cooldown.
#[derive(Debug, Default)]
struct TemplateState {
    current_level: AnomalyLevel,
    last_emitted_us: Option<u64>,
    /// Level der letzten tatsächlich gemeldeten Anomalie. Eine Verschärfung
    /// darüber hinaus durchbricht die Sperrzeit.
    last_emitted_level: AnomalyLevel,
    suppressed_since_last: u64,
}


/// Statistik über die Arbeit der Engine, für Anzeige und Diagnose.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AnalysisStats {
    /// Verarbeitete Ereignisse insgesamt.
    pub processed: u64,
    /// Gemeldete Anomalien.
    pub emitted: u64,
    /// Wegen Cooldown/Dedup unterdrückte Meldungen.
    pub suppressed: u64,
    /// Während der Lernphase unterdrückte Meldungen.
    pub suppressed_learning: u64,
}

/// Die Analyse-Engine. Hält Zeitfenster, Rate-Historie, Entropie-Historie,
/// die Zeitprofil-Baselines (Phase 4) und den Meldezustand pro Template.
pub struct AnalysisEngine {
    config: AnalysisConfig,
    window: SlidingWindow,
    rates: RateTracker,
    /// Zeitprofil-Baselines pro Unit (Phase 4): erste Quelle für Rate-
    /// Z-Score und Surprisal, bevor auf Kurzzeit-Historie bzw.
    /// Momentanfenster zurückgefallen wird.
    baselines: BaselineStore,
    /// Aufbewahrt für [`AnalysisEngine::restore_baselines`], das einen neuen
    /// [`BaselineStore`] mit denselben Einstellungen braucht.
    baseline_config: BaselineConfig,
    /// Historie der Fenster-Entropie, ein Wert je abgeschlossenem Bucket.
    entropy_history: Vec<f64>,
    entropy_bucket: u64,
    bucket_us: u64,
    states: HashMap<TemplateId, TemplateState>,
    /// Zeitstempel des allerersten Ereignisses; Beginn der Lernphase.
    first_event_us: Option<u64>,
    /// Wurde die globale Lernphase übersprungen, weil beim Start
    /// vertrauenswürdige Baselines geladen wurden?
    learning_skipped: bool,
    stats: AnalysisStats,
}

impl AnalysisEngine {
    /// Erstellt eine Engine aus der Analyse-Konfiguration, mit
    /// Default-Einstellungen für die Zeitprofil-Baselines (Phase 4). Für
    /// explizite Baseline-Konfiguration siehe
    /// [`AnalysisEngine::with_baseline_config`].
    pub fn new(config: AnalysisConfig) -> Self {
        Self::with_baseline_config(config, BaselineConfig::default())
    }

    /// Erstellt eine Engine mit expliziter Baseline-Konfiguration.
    pub fn with_baseline_config(config: AnalysisConfig, baseline_config: BaselineConfig) -> Self {
        let window = SlidingWindow::new(config.window_seconds, config.max_window_events);
        let rates = RateTracker::new(
            config.bucket_seconds,
            config.rate_history_buckets,
            config.max_templates,
            config.idle_eviction_buckets,
        );
        let baselines = BaselineStore::new(config.bucket_seconds, &baseline_config);
        let bucket_us = config.bucket_seconds.max(1).saturating_mul(1_000_000);
        Self {
            config,
            window,
            rates,
            baselines,
            baseline_config,
            entropy_history: Vec::new(),
            entropy_bucket: 0,
            bucket_us,
            states: HashMap::new(),
            first_event_us: None,
            learning_skipped: false,
            stats: AnalysisStats::default(),
        }
    }

    /// Setzt die Zeitprofil-Baselines auf einen zuvor gesicherten Stand
    /// zurück (Neustart mit persistierten Daten, Schritt 9). Der bisherige
    /// Baseline-Zustand geht verloren; alles andere (Kurzzeit-Historie,
    /// Meldezustand) bleibt unverändert.
    pub fn restore_baselines(&mut self, snapshot: crate::baseline::BaselineSnapshot) {
        self.baselines = BaselineStore::restore(snapshot, self.config.bucket_seconds, &self.baseline_config);
    }

    /// Serialisierbare Kopie der aktuellen Zeitprofil-Baselines (Schritt 9).
    pub fn baseline_snapshot(&self) -> crate::baseline::BaselineSnapshot {
        self.baselines.snapshot()
    }

    /// Erzwingt die Kapazitätsgrenze der Baselines (Regel 18). Der Daemon
    /// ruft dies vor jedem Snapshot auf, damit verworfene Einträge auch aus
    /// der Datei verschwinden.
    pub fn enforce_baseline_limits(&mut self) {
        self.baselines.enforce_limits();
    }

    /// Ob mindestens eine Zeitprofil-Baseline bereits vertrauenswürdig ist.
    /// Der Daemon nutzt dies, um nach dem Laden persistierter Baselines die
    /// globale Lernphase zu überspringen (Schritt 9).
    pub fn has_trusted_baselines(&self) -> bool {
        self.baselines.has_trusted_baselines()
    }

    /// Laufende Kennzahlen der Engine.
    pub fn stats(&self) -> AnalysisStats {
        self.stats
    }

    /// Aktuelle Entropie des Zeitfensters in Bit (für die Kopfzeile der GUI).
    pub fn current_entropy(&self) -> f64 {
        self.window.entropy()
    }

    /// Anzahl Ereignisse im aktuellen Zeitfenster.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Anzahl unterschiedlicher Templates im aktuellen Zeitfenster (für den
    /// Snapshot, Phase 6).
    pub fn distinct_templates(&self) -> usize {
        self.window.distinct_templates()
    }

    /// Robuster Z-Score der aktuellen Fenster-Entropie gegen ihre Historie,
    /// unabhängig von einem konkreten Ereignis -- für die periodische
    /// Snapshot-Veröffentlichung (Phase 6), die nicht an ein `process()`
    /// gekoppelt ist. Dieselbe Formel wie in [`Self::process`].
    pub fn current_entropy_z(&self) -> f64 {
        robust_z_score(self.window.entropy(), &self.entropy_history, ENTROPY_MIN_SCALE)
    }

    /// Konfigurierte Fenstergröße in Sekunden (Kopfzeile der GUI).
    pub fn window_seconds(&self) -> u64 {
        self.config.window_seconds
    }

    /// Verbleibende Sekunden der globalen Lernphase zum gegebenen Zeitpunkt,
    /// `None` außerhalb der Lernphase.
    pub fn learning_remaining_secs(&self, timestamp_us: u64) -> Option<u64> {
        if !self.in_learning_phase(timestamp_us) {
            return None;
        }
        let start = self.first_event_us?;
        let learning_us = self
            .config
            .learning_phase_minutes
            .saturating_mul(60)
            .saturating_mul(1_000_000);
        Some(learning_us.saturating_sub(timestamp_us.saturating_sub(start)) / 1_000_000)
    }

    /// Überspringt die globale Lernphase. Sinnvoll nur, wenn beim Start
    /// vertrauenswürdige Baselines geladen wurden -- dann ist die Lernphase
    /// als Kaltstart-Schutz nicht mehr nötig, und wochenlang gelernte
    /// Normalität würde sonst zehn Minuten lang ignoriert.
    pub fn skip_learning_phase(&mut self) {
        self.learning_skipped = true;
    }

    /// Ob die Lernphase zum gegebenen Zeitpunkt noch läuft.
    pub fn in_learning_phase(&self, timestamp_us: u64) -> bool {
        if self.learning_skipped {
            return false;
        }
        let Some(start) = self.first_event_us else {
            return true;
        };
        let learning_us = self
            .config
            .learning_phase_minutes
            .saturating_mul(60)
            .saturating_mul(1_000_000);
        timestamp_us.saturating_sub(start) < learning_us
    }

    /// Verarbeitet ein Ereignis und liefert eine Anomalie, falls eine zu
    /// melden ist.
    ///
    /// Der Zustand wird auch während der Lernphase und bei unterdrückten
    /// Meldungen vollständig fortgeschrieben – gelernt wird immer, gemeldet
    /// nur, wenn alle Dämpfungen es zulassen.
    pub fn process(&mut self, input: AnalysisInput<'_>) -> Option<Anomaly> {
        self.stats.processed += 1;
        if self.first_event_us.is_none() {
            self.first_event_us = Some(input.timestamp_us);
            self.entropy_bucket = input.timestamp_us / self.bucket_us;
        }

        // Reihenfolge ist wichtig: erst die Entropie des *bisherigen*
        // Fensters archivieren, dann das neue Ereignis aufnehmen. Sonst
        // verglichen wir den aktuellen Wert mit einer Historie, die ihn
        // bereits enthält.
        self.roll_entropy_bucket(input.timestamp_us);

        let unit_key = unit_key_from_name(input.unit);

        // Kurzzeit-Historie (Phase 3) läuft immer mit -- sie ist die
        // Fallback-Stufe, falls die Zeitprofil-Baseline noch nicht
        // vertrauenswürdig ist (frisches System, wenig Beobachtungen).
        let short_term_rate_z = self.rates.record(input.timestamp_us, input.template_id);
        self.window.push(input.timestamp_us, input.template_id);

        // Zeitprofil-Baseline (Phase 4) zählt das Ereignis mit und liefert,
        // sofern vertrauenswürdig, den bevorzugten Rate-Z-Score.
        self.baselines.record(input.timestamp_us, unit_key, input.template_id);
        let current_bucket_count = self.baselines.current_bucket_count(unit_key, input.template_id);
        let baseline_rate =
            self.baselines
                .rate_z(input.timestamp_us, unit_key, input.template_id, current_bucket_count);

        let breakdown =
            self.compute_breakdown(unit_key, input.template_id, short_term_rate_z, baseline_rate);
        let level = self.level_with_hysteresis(input.template_id, breakdown.combined);

        if level == AnomalyLevel::Normal {
            return None;
        }

        if self.in_learning_phase(input.timestamp_us) {
            self.stats.suppressed_learning += 1;
            return None;
        }

        if self.is_in_cooldown(input.template_id, input.timestamp_us, level) {
            self.stats.suppressed += 1;
            if let Some(state) = self.states.get_mut(&input.template_id) {
                state.suppressed_since_last += 1;
            }
            return None;
        }

        let suppressed_since_last = {
            let state = self.states.entry(input.template_id).or_default();
            state.last_emitted_us = Some(input.timestamp_us);
            state.last_emitted_level = level;
            std::mem::take(&mut state.suppressed_since_last)
        };

        self.stats.emitted += 1;
        Some(Anomaly {
            timestamp_us: input.timestamp_us,
            template_id: input.template_id,
            unit: input.unit.map(str::to_string),
            level,
            breakdown,
            suppressed_since_last,
        })
    }

    /// Archiviert die Fenster-Entropie, sobald ein Bucket abgeschlossen ist.
    fn roll_entropy_bucket(&mut self, timestamp_us: u64) {
        let bucket = timestamp_us / self.bucket_us;
        if bucket <= self.entropy_bucket {
            return;
        }
        if !self.window.is_empty() {
            let entropy = self.window.entropy();
            if self.entropy_history.len() >= self.config.rate_history_buckets {
                self.entropy_history.remove(0);
            }
            self.entropy_history.push(entropy);
        }
        self.entropy_bucket = bucket;
    }

    /// Berechnet die Einzelsignale und den gewichteten Gesamtscore.
    ///
    /// `baseline_rate` ist das Ergebnis der Zeitprofil-Baseline (Phase 4,
    /// Stufe 1-2 der Fallback-Kette); ist es `None`, wird
    /// `short_term_rate_z` aus der Kurzzeit-Historie (Phase 3, Stufe 3)
    /// verwendet.
    fn compute_breakdown(
        &self,
        unit_key: u64,
        template_id: TemplateId,
        short_term_rate_z: f64,
        baseline_rate: Option<(f64, RateSource)>,
    ) -> ScoreBreakdown {
        let (rate_z, rate_source) = match baseline_rate {
            Some((z, source)) => (z, source),
            None => (short_term_rate_z, RateSource::ShortTerm),
        };

        // Surprisal ebenfalls zuerst aus dem langfristigen Unit-Profil
        // (Phase 4); das ist die gezielte Antwort auf die in Phase 3
        // dokumentierte sturmkorrelierte Fehlalarmquelle: Ein Sturm
        // verändert ein mit Tagen Halbwertszeit gewichtetes Profil nur
        // marginal, während er das Momentanfenster vollständig dominiert.
        let surprisal_bits = self
            .baselines
            .surprisal(unit_key, template_id, self.config.surprisal_smoothing_alpha)
            .unwrap_or_else(|| {
                surprisal(
                    self.window.count_of(template_id),
                    self.window.len() as u64,
                    self.window.distinct_templates(),
                    self.config.surprisal_smoothing_alpha,
                )
            });

        let entropy_z = robust_z_score(
            self.window.entropy(),
            &self.entropy_history,
            ENTROPY_MIN_SCALE,
        );

        // Nur Ausschläge nach oben sind für die Rate relevant: dass ein
        // Template seltener wird, ist kein Alarmgrund. Bei der Entropie
        // zählt dagegen der Betrag, weil beide Richtungen auffällig sind
        // (Einbruch = Log-Sturm, Anstieg = Wildwuchs).
        let rate_component = normalize(rate_z.max(0.0), self.config.rate_z_reference);
        let surprisal_component =
            normalize(surprisal_bits, self.config.surprisal_reference_bits);

        // Die Entropie ist ein *systemweites* Signal: sie sagt, dass die
        // Mischung im Fenster ungewöhnlich ist, aber nicht, wer sie
        // verursacht. Ordnet man sie unverändert jedem Ereignis zu, meldet
        // ein Log-Sturm einer Unit auch sämtliche unbeteiligten Units als
        // anomal. Deshalb wird der Entropie-Anteil mit dem Fensteranteil des
        // Templates gewichtet: Wer den Sturm verursacht, trägt ihn auch.
        let share = if !self.window.is_empty() {
            self.window.count_of(template_id) as f64 / self.window.len() as f64
        } else {
            0.0
        };
        let entropy_component =
            normalize(entropy_z.abs(), self.config.entropy_z_reference) * share;

        let combined = noisy_or(&[
            (self.config.weight_rate, rate_component),
            (self.config.weight_surprisal, surprisal_component),
            (self.config.weight_entropy, entropy_component),
        ]);

        ScoreBreakdown {
            rate_z,
            surprisal_bits,
            entropy_z,
            rate_component,
            surprisal_component,
            entropy_component,
            combined: combined.clamp(0.0, 1.0),
            rate_source,
        }
    }

    /// Bestimmt das Level unter Berücksichtigung der Hysterese und
    /// aktualisiert den gespeicherten Zustand.
    fn level_with_hysteresis(&mut self, template_id: TemplateId, score: f64) -> AnomalyLevel {
        let exit_factor = self.config.hysteresis_exit_factor.clamp(0.0, 1.0);
        let warn = self.config.score_warn_threshold;
        let critical = self.config.score_critical_threshold;
        let info = self.config.score_info_threshold;

        let previous = self
            .states
            .get(&template_id)
            .map_or(AnomalyLevel::Normal, |s| s.current_level);

        // Einstieg über die volle Schwelle, Ausstieg erst unter der um den
        // Faktor gesenkten Schwelle. Dazwischen bleibt das Level bestehen.
        let raw_level = if score >= critical {
            AnomalyLevel::Critical
        } else if score >= warn {
            AnomalyLevel::Warn
        } else if score >= info {
            AnomalyLevel::Info
        } else {
            AnomalyLevel::Normal
        };

        let held_level = match previous {
            AnomalyLevel::Critical if score >= critical * exit_factor => AnomalyLevel::Critical,
            AnomalyLevel::Warn if score >= warn * exit_factor => AnomalyLevel::Warn,
            AnomalyLevel::Info if score >= info * exit_factor => AnomalyLevel::Info,
            _ => AnomalyLevel::Normal,
        };

        let level = raw_level.max(held_level);
        self.states.entry(template_id).or_default().current_level = level;
        level
    }

    /// Prüft, ob für dieses Template noch eine Sperrzeit läuft.
    ///
    /// Eine **Verschärfung** durchbricht die Sperrzeit: Meldet sich ein
    /// Template erneut mit einem höheren Level als bei der letzten Meldung,
    /// wird es durchgelassen. Ohne diese Ausnahme friert die erste, meist
    /// schwächste Erkennung das Urteil ein – ein Angriff, der zunächst nur
    /// über das Surprisal auffällt und erst Sekunden später eine erdrückende
    /// Rate-Evidenz liefert, bliebe für die gesamte Sperrzeit auf `info`
    /// stehen, obwohl er längst `critical` wäre.
    fn is_in_cooldown(
        &self,
        template_id: TemplateId,
        timestamp_us: u64,
        level: AnomalyLevel,
    ) -> bool {
        let Some(state) = self.states.get(&template_id) else {
            return false;
        };
        let Some(last) = state.last_emitted_us else {
            return false;
        };
        if level > state.last_emitted_level {
            return false;
        }
        let cooldown_us = self.config.cooldown_seconds.saturating_mul(1_000_000);
        timestamp_us.saturating_sub(last) < cooldown_us
    }
}

/// Normiert einen Rohwert auf 0..1 anhand eines Referenzwerts.
fn normalize(value: f64, reference: f64) -> f64 {
    if reference <= 0.0 {
        return 0.0;
    }
    (value / reference).clamp(0.0, 1.0)
}

/// Noisy-OR-Verknüpfung gewichteter Signale: `1 - Π (1 - wᵢ·cᵢ)`.
///
/// Ergebnis liegt garantiert in 0..1 und ist in jedem Einzelsignal monoton
/// steigend. Ein einzelnes voll ausgeschlagenes Signal mit Gewicht 1.0
/// treibt den Score auf 1.0, unabhängig von den übrigen.
fn noisy_or(signals: &[(f64, f64)]) -> f64 {
    let complement: f64 = signals
        .iter()
        .map(|(weight, component)| {
            let w = weight.clamp(0.0, 1.0);
            let c = component.clamp(0.0, 1.0);
            1.0 - w * c
        })
        .product();
    (1.0 - complement).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000;

    fn tid(n: u64) -> TemplateId {
        TemplateId(n)
    }

    /// Konfiguration mit sehr kurzer Lernphase, damit Tests nicht minutenlang
    /// „aufwärmen" müssen.
    fn test_config() -> AnalysisConfig {
        AnalysisConfig {
            learning_phase_minutes: 0,
            cooldown_seconds: 60,
            ..AnalysisConfig::default()
        }
    }

    fn input(timestamp_us: u64, template: u64) -> AnalysisInput<'static> {
        AnalysisInput {
            timestamp_us,
            template_id: tid(template),
            unit: None,
        }
    }

    #[test]
    fn normalize_deckelt_auf_eins() {
        assert!((normalize(10.0, 5.0) - 1.0).abs() < 1e-9);
        assert!((normalize(2.5, 5.0) - 0.5).abs() < 1e-9);
        assert!(normalize(-3.0, 5.0).abs() < 1e-9);
        assert!(normalize(1.0, 0.0).abs() < 1e-9);
    }

    #[test]
    fn lernphase_unterdrueckt_meldungen() {
        let config = AnalysisConfig {
            learning_phase_minutes: 10,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);

        // Selbst ein massiver Burst darf während der Lernphase nichts melden.
        for i in 0..500 {
            let result = engine.process(input(i * 1000, 1));
            assert!(result.is_none(), "während der Lernphase darf nichts gemeldet werden");
        }
        assert!(engine.stats().emitted == 0);
    }

    #[test]
    fn lernphase_endet_nach_konfigurierter_dauer() {
        let config = AnalysisConfig {
            learning_phase_minutes: 1,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        engine.process(input(0, 1));
        assert!(engine.in_learning_phase(30 * SEC));
        assert!(!engine.in_learning_phase(120 * SEC));
    }

    #[test]
    fn skip_learning_phase_beendet_die_lernphase_sofort() {
        let config = AnalysisConfig {
            learning_phase_minutes: 60,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        assert!(engine.in_learning_phase(0));
        engine.skip_learning_phase();
        assert!(!engine.in_learning_phase(0));
        engine.process(input(0, 1));
        assert!(!engine.in_learning_phase(SEC));
    }

    #[test]
    fn learning_remaining_secs_ist_none_ausserhalb_der_lernphase() {
        let config = AnalysisConfig {
            learning_phase_minutes: 1,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        engine.process(input(0, 1));
        assert_eq!(engine.learning_remaining_secs(120 * SEC), None);
    }

    #[test]
    fn learning_remaining_secs_zaehlt_bis_zum_ende_der_lernphase_herunter() {
        let config = AnalysisConfig {
            learning_phase_minutes: 1,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        engine.process(input(0, 1));
        assert_eq!(engine.learning_remaining_secs(10 * SEC), Some(50));
    }

    #[test]
    fn distinct_templates_und_entropy_z_spiegeln_das_fenster_ohne_ereignis() {
        let mut engine = AnalysisEngine::new(test_config());
        assert_eq!(engine.distinct_templates(), 0);
        engine.process(input(0, 1));
        engine.process(input(1, 2));
        assert_eq!(engine.distinct_templates(), 2);
        // Mit leerer Historie ist der Z-Score nicht NaN/unendlich, sondern
        // per Konvention der neutrale Wert der robusten Skala.
        assert!(engine.current_entropy_z().is_finite());
    }

    #[test]
    fn window_seconds_liefert_konfigurierte_fenstergroesse() {
        let config = AnalysisConfig {
            window_seconds: 42,
            ..AnalysisConfig::default()
        };
        let engine = AnalysisEngine::new(config);
        assert_eq!(engine.window_seconds(), 42);
    }

    #[test]
    fn ruhiger_normalbetrieb_meldet_nichts() {
        let mut engine = AnalysisEngine::new(test_config());
        // Gleichmäßiger Mix aus vier Templates über 10 Minuten.
        let mut anomalies = 0;
        for i in 0..600u64 {
            for t in 0..4u64 {
                if engine.process(input(i * SEC, t)).is_some() {
                    anomalies += 1;
                }
            }
        }
        assert_eq!(
            anomalies, 0,
            "gleichmäßiger Normalbetrieb darf keine Anomalien erzeugen"
        );
    }

    #[test]
    fn noisy_or_laesst_einzelnes_starkes_signal_durchschlagen() {
        // Genau der Fall, an dem eine gewichtete Mittelung scheitert: nur
        // ein Signal schlägt aus, muss aber den Score tragen können.
        let score = noisy_or(&[(1.0, 0.0), (0.8, 1.0), (0.4, 0.0)]);
        assert!((score - 0.8).abs() < 1e-9, "war {score}");
    }

    #[test]
    fn noisy_or_bleibt_zwischen_null_und_eins_und_ist_monoton() {
        assert!(noisy_or(&[(1.0, 0.0), (1.0, 0.0)]).abs() < 1e-9);
        assert!((noisy_or(&[(1.0, 1.0), (1.0, 1.0)]) - 1.0).abs() < 1e-9);
        let schwach = noisy_or(&[(1.0, 0.2), (0.8, 0.1)]);
        let stark = noisy_or(&[(1.0, 0.5), (0.8, 0.1)]);
        assert!(stark > schwach, "muss monoton steigen");
    }

    #[test]
    fn plötzlicher_burst_eines_neuen_templates_wird_gemeldet() {
        let mut engine = AnalysisEngine::new(test_config());
        // Ruhige Grundlast über 10 Minuten.
        for i in 0..600u64 {
            for t in 0..4u64 {
                engine.process(input(i * SEC, t));
            }
        }
        // Dann ein bisher nie gesehenes Template.
        let mut found = None;
        for i in 0..200u64 {
            if let Some(anomaly) = engine.process(input(600 * SEC + i * 100_000, 7)) {
                found = Some(anomaly);
                break;
            }
        }
        let anomaly = found.expect("neues Template im Burst muss eine Anomalie auslösen");
        assert!(anomaly.level >= AnomalyLevel::Info);
        assert!(
            anomaly.breakdown.surprisal_bits > 5.0,
            "das Surprisal muss der treibende Anteil sein, war {}",
            anomaly.breakdown.surprisal_bits
        );
    }

    #[test]
    fn cooldown_verhindert_wiederholte_meldungen() {
        let config = AnalysisConfig {
            learning_phase_minutes: 0,
            cooldown_seconds: 3600,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        for i in 0..600u64 {
            for t in 0..4u64 {
                engine.process(input(i * SEC, t));
            }
        }

        let mut level_verlauf = Vec::new();
        for i in 0..500u64 {
            if let Some(a) = engine.process(input(600 * SEC + i * 100_000, 7)) {
                level_verlauf.push(a.level);
            }
        }

        // Innerhalb der Sperrzeit darf dasselbe Level nicht wiederholt
        // melden; nur echte Verschärfungen kommen durch (siehe
        // `verschaerfung_durchbricht_die_sperrzeit`). Es kann also höchstens
        // je eine Meldung pro Stufe geben.
        assert!(
            level_verlauf.len() <= 3,
            "höchstens eine Meldung je Stufe erwartet, war {level_verlauf:?}"
        );
        assert!(
            level_verlauf.windows(2).all(|paar| paar[1] > paar[0]),
            "aufeinanderfolgende Meldungen müssen echte Verschärfungen sein, war {level_verlauf:?}"
        );
        assert!(
            engine.stats().suppressed > 0,
            "die übrigen Ereignisse müssen unterdrückt worden sein"
        );
    }

    #[test]
    fn verschaerfung_durchbricht_die_sperrzeit() {
        let config = AnalysisConfig {
            learning_phase_minutes: 0,
            // Sperrzeit weit länger als der gesamte Testverlauf.
            cooldown_seconds: 86_400,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        for i in 0..600u64 {
            for t in 0..4u64 {
                engine.process(input(i * SEC, t));
            }
        }

        // Anhaltender Sturm eines neuen Templates: die erste Meldung fällt
        // schwach aus, die Evidenz wächst danach stark an.
        let mut level_verlauf = Vec::new();
        for i in 0..600u64 {
            if let Some(a) = engine.process(input(600 * SEC + i * 50_000, 7)) {
                level_verlauf.push(a.level);
            }
        }

        assert!(
            level_verlauf.len() >= 2,
            "trotz Sperrzeit muss eine Verschärfung durchkommen, war {level_verlauf:?}"
        );
        assert!(
            level_verlauf.last() > level_verlauf.first(),
            "das Level muss sich verschärfen, war {level_verlauf:?}"
        );
        // Umgekehrt darf sich dasselbe Level nicht wiederholen.
        let mut sortiert = level_verlauf.clone();
        sortiert.dedup();
        assert_eq!(
            sortiert.len(),
            level_verlauf.len(),
            "gleiches Level darf innerhalb der Sperrzeit nicht erneut melden"
        );
    }

    #[test]
    fn unterdrueckte_meldungen_werden_gezaehlt_und_weitergereicht() {
        let config = AnalysisConfig {
            learning_phase_minutes: 0,
            cooldown_seconds: 30,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        for i in 0..600u64 {
            for t in 0..4u64 {
                engine.process(input(i * SEC, t));
            }
        }

        // Wiederkehrende Bursts eines sonst stillen Templates, weit genug
        // auseinander, dass die Sperrzeit dazwischen abläuft.
        let mut anomalies = Vec::new();
        for burst in 0..4u64 {
            let base = (600 + burst * 120) * SEC;
            for s in 0..120u64 {
                for t in 0..4u64 {
                    engine.process(input(base + s * SEC, t));
                }
            }
            for i in 0..30u64 {
                if let Some(a) = engine.process(input(base + 60 * SEC + i * 100_000, 7)) {
                    anomalies.push(a);
                }
            }
        }

        assert!(
            anomalies.len() >= 2,
            "nach Ablauf der Sperrzeit sollte erneut gemeldet werden, waren {}",
            anomalies.len()
        );
        assert!(
            anomalies[1].suppressed_since_last > 0,
            "die Zahl der unterdrückten Meldungen muss weitergereicht werden"
        );
        assert!(engine.stats().suppressed > 0);
    }

    #[test]
    fn hysterese_haelt_level_bis_deutlich_unter_die_schwelle() {
        let config = AnalysisConfig {
            score_info_threshold: 0.3,
            score_warn_threshold: 0.5,
            score_critical_threshold: 0.8,
            hysteresis_exit_factor: 0.8,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);

        // Direkt über die Warn-Schwelle.
        let level = engine.level_with_hysteresis(tid(1), 0.55);
        assert_eq!(level, AnomalyLevel::Warn);

        // Knapp darunter (0.45 >= 0.5*0.8 = 0.4): Level wird gehalten.
        let level = engine.level_with_hysteresis(tid(1), 0.45);
        assert_eq!(level, AnomalyLevel::Warn, "Hysterese muss das Level halten");

        // Deutlich darunter: Level fällt zurück.
        let level = engine.level_with_hysteresis(tid(1), 0.1);
        assert_eq!(level, AnomalyLevel::Normal);
    }

    #[test]
    fn hoehere_scores_ergeben_hoehere_level() {
        let config = AnalysisConfig {
            score_info_threshold: 0.3,
            score_warn_threshold: 0.5,
            score_critical_threshold: 0.8,
            ..AnalysisConfig::default()
        };
        let mut engine = AnalysisEngine::new(config);
        assert_eq!(engine.level_with_hysteresis(tid(1), 0.1), AnomalyLevel::Normal);
        assert_eq!(engine.level_with_hysteresis(tid(2), 0.35), AnomalyLevel::Info);
        assert_eq!(engine.level_with_hysteresis(tid(3), 0.6), AnomalyLevel::Warn);
        assert_eq!(engine.level_with_hysteresis(tid(4), 0.9), AnomalyLevel::Critical);
    }

    #[test]
    fn score_bleibt_immer_zwischen_null_und_eins() {
        let mut engine = AnalysisEngine::new(test_config());
        for i in 0..300u64 {
            engine.process(input(i * SEC, i % 5));
        }
        for i in 0..300u64 {
            let breakdown = engine.compute_breakdown(0, tid(i % 7), 1000.0, None);
            assert!(
                (0.0..=1.0).contains(&breakdown.combined),
                "Score außerhalb 0..1: {}",
                breakdown.combined
            );
        }
    }

    #[test]
    fn engine_paniked_nicht_bei_pathologischen_eingaben() {
        // Regel 16: der Analysepfad darf unter keinen Umständen paniken.
        let mut engine = AnalysisEngine::new(test_config());
        engine.process(input(0, 1));
        engine.process(input(u64::MAX, 1));
        engine.process(input(0, 1));
        engine.process(input(u64::MAX / 2, 999_999));
        assert!(engine.stats().processed == 4);
    }
}
