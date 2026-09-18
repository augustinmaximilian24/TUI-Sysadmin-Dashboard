//! In-Memory-Zustand aller Baselines: Slot-Histogramme, Unit-Profile und
//! die offenen (noch nicht abgeschlossenen) Bucket-Zähler je Unit.
//!
//! Reines Rust, kein I/O -- Persistenz kommt erst in Schritt 8
//! ([`super::persist`]). Siehe `docs/phase4-baselines.md` Abschnitt 8 für
//! den Signaturvertrag.
//!
//! Abweichung vom Vertrag: [`BaselineStore::new`] und
//! [`BaselineStore::restore`] erhalten zusätzlich `bucket_seconds`, da
//! [`crate::config::BaselineConfig`] diesen Wert bewusst nicht dupliziert
//! (er lebt in `AnalysisConfig`, wird von `RateTracker` und `BaselineStore`
//! gemeinsam genutzt).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::analysis::stats::{z_score_from_median_mad, COUNT_MIN_SCALE};
use crate::config::BaselineConfig;
use crate::template::TemplateId;

use super::decay::DecayParams;
use super::histogram::{CountHistogram, HistogramConfig};
use super::profile::{ProfileConfig, UnitProfile};
use super::slot::Slot;

/// Reservierter Name für Ereignisse ohne `_SYSTEMD_UNIT`
/// (`docs/phase4-baselines.md` Abschnitt 3).
const NO_UNIT_NAME: &str = "<none>";

/// Bildet den stabilen Unit-Schlüssel aus dem (optionalen) Unit-Namen.
/// Fehlt der Name, wird der reservierte Platzhalter [`NO_UNIT_NAME`]
/// gehasht, damit auch unit-lose Ereignisse konsistent gruppiert werden.
pub fn unit_key_from_name(unit: Option<&str>) -> u64 {
    crate::hash::fnv1a_hash64(unit.unwrap_or(NO_UNIT_NAME).as_bytes())
}

/// Zusammengesetzter Schlüssel einer einzelnen Slot-Baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaselineKey {
    /// Hash des Unit-Namens (siehe `docs/phase4-baselines.md` Abschnitt 3).
    pub unit_key: u64,
    /// Betroffenes Template.
    pub template_id: TemplateId,
    /// Zeitprofil-Slot, für den diese Baseline gilt.
    pub slot: Slot,
}

/// Woher der zurückgelieferte Rate-Z-Score stammt (Fallback-Kette,
/// `docs/phase4-baselines.md` Abschnitt 5).
///
/// [`BaselineStore::rate_z`] liefert selbst nur `SlotBaseline` oder
/// `AnySlotBaseline` (als `Some`) bzw. `None` (kein `Some`-Wert). Die
/// Varianten `ShortTerm` und `None` gehören zur vollständigen Kette und
/// werden erst von der Analyse-Engine (Schritt 7) gesetzt, wenn auch der
/// `BaselineStore` nichts Vertrauenswürdiges liefert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSource {
    /// Aus der Baseline des exakten Zeitprofil-Slots.
    SlotBaseline,
    /// Aus der Sammel-Baseline über alle Slots hinweg ([`Slot::ANY`]).
    AnySlotBaseline,
    /// Aus der Kurzzeit-Historie (`RateTracker`, Phase 3).
    ShortTerm,
    /// Kein Signal verfügbar.
    None,
}

/// Serialisierbarer Schnappschuss des gesamten Baseline-Zustands.
///
/// Bewusst als flache `Vec` statt verschachtelter `HashMap`, damit die
/// JSON-Repräsentation unabhängig von `HashMap`s nicht garantierter
/// Iterationsreihenfolge ist und sich 1:1 auf redb-Tabellenzeilen abbilden
/// lässt (Schritt 8).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct BaselineSnapshot {
    /// Alle Slot-Histogramme (inklusive der `Slot::ANY`-Sammelbaselines).
    pub histograms: Vec<(BaselineKey, CountHistogram)>,
    /// Alle Unit-Profile, je Unit-Schlüssel.
    pub profiles: Vec<(u64, UnitProfile)>,
}

/// Offener, noch nicht in ein Histogramm übernommener Bucket-Zustand einer
/// einzelnen Unit.
#[derive(Debug, Default)]
struct UnitBucketState {
    open_bucket: Option<u64>,
    open_slot: Slot,
    /// Zählwerte je Template im aktuell laufenden Bucket.
    bucket_counts: HashMap<TemplateId, u32>,
    /// Jedes Template, das für diese Unit jemals beobachtet wurde, mit dem
    /// Bucket seiner letzten Aktivität (für Nullbeobachtungen beim
    /// Bucket-Abschluss und für die Verdrängung bei Kapazitätsdruck).
    known_templates: HashMap<TemplateId, u64>,
    /// Bucket der letzten Aktivität dieser Unit (für die Verdrängung ganzer
    /// Units bei Kapazitätsdruck, siehe `BaselineStore::enforce_limits`).
    last_touched_bucket: u64,
}

impl Default for Slot {
    fn default() -> Self {
        Slot::ANY
    }
}

/// Verwaltet Slot-Histogramme, Unit-Profile und die offenen Bucket-Zähler.
pub struct BaselineStore {
    histograms: HashMap<BaselineKey, CountHistogram>,
    unit_profiles: HashMap<u64, UnitProfile>,
    unit_states: HashMap<u64, UnitBucketState>,
    bucket_us: u64,
    hist_config: HistogramConfig,
    profile_config: ProfileConfig,
    min_weight: f64,
    profile_min_weight: f64,
    max_baselines: usize,
    /// Obergrenze der pro Unit als "bekannt" geführten Templates. Nutzt
    /// denselben Konfigurationswert wie `profile_max_templates`, da beide
    /// dieselbe Frage beantworten ("wie viele unterschiedliche Templates
    /// verfolgen wir für diese Unit aktiv") und eine zusätzliche
    /// Konfigurationsoption hier keinen praktischen Nutzen hätte.
    max_known_templates: usize,
}

impl BaselineStore {
    /// Erstellt einen leeren Store aus der Konfiguration.
    pub fn new(bucket_seconds: u64, config: &BaselineConfig) -> Self {
        let decay = DecayParams::from_hours(config.half_life_hours, bucket_seconds);
        Self {
            histograms: HashMap::new(),
            unit_profiles: HashMap::new(),
            unit_states: HashMap::new(),
            bucket_us: bucket_seconds.max(1).saturating_mul(1_000_000),
            hist_config: HistogramConfig {
                decay,
                max_bins: config.max_bins,
            },
            profile_config: ProfileConfig {
                decay,
                max_templates: config.profile_max_templates,
            },
            min_weight: config.min_weight,
            profile_min_weight: config.profile_min_weight,
            max_baselines: config.max_baselines,
            max_known_templates: config.profile_max_templates,
        }
    }

    /// Zählt ein Ereignis. Aktualisiert das Unit-Profil sofort (es misst
    /// eine Häufigkeitsverteilung, keine Bucket-Rate) und trägt den
    /// Zählwert in den offenen Bucket der Unit ein. Wechselt das Ereignis
    /// in einen neuen Bucket, wird der vorherige zuvor abgeschlossen --
    /// inklusive Nullbeobachtungen für bekannte Templates, die in diesem
    /// (aktiven) Bucket nicht auftraten (Abschnitt 3.3 des Entwurfs).
    pub fn record(&mut self, timestamp_us: u64, unit_key: u64, template: TemplateId) {
        let bucket = timestamp_us / self.bucket_us;
        let slot = Slot::from_timestamp_us(timestamp_us);

        self.unit_profiles.entry(unit_key).or_default().observe(
            template,
            bucket,
            &self.profile_config,
        );

        let hist_config = self.hist_config;
        let max_known = self.max_known_templates;
        let histograms = &mut self.histograms;
        let state = self.unit_states.entry(unit_key).or_default();
        state.last_touched_bucket = state.last_touched_bucket.max(bucket);

        match state.open_bucket {
            Some(prev) if bucket > prev => {
                close_bucket(histograms, unit_key, state, prev, &hist_config);
                state.bucket_counts.clear();
                state.open_bucket = Some(bucket);
                state.open_slot = slot;
            }
            None => {
                state.open_bucket = Some(bucket);
                state.open_slot = slot;
            }
            // Gleicher oder früherer Bucket (verspätetes Ereignis): dem
            // laufenden Bucket zuschlagen, wie an anderer Stelle im Projekt
            // (siehe `daemon::ingestion`) für verspätete Journal-Zeilen.
            _ => {}
        }

        *state.bucket_counts.entry(template).or_insert(0) += 1;
        state.known_templates.insert(template, bucket);
        evict_known_if_needed(state, max_known);
    }

    /// Robuster Z-Score der aktuell laufenden Bucket-Rate gegenüber der
    /// Baseline, aus der ersten vertrauenswürdigen Quelle: erst der exakte
    /// Zeitprofil-Slot, dann die Sammel-Baseline über alle Slots.
    pub fn rate_z(
        &self,
        timestamp_us: u64,
        unit_key: u64,
        template: TemplateId,
        current_count: u32,
    ) -> Option<(f64, RateSource)> {
        let slot = Slot::from_timestamp_us(timestamp_us);

        if let Some(z) = self.z_from_slot(unit_key, template, slot, current_count) {
            return Some((z, RateSource::SlotBaseline));
        }
        if slot != Slot::ANY {
            if let Some(z) = self.z_from_slot(unit_key, template, Slot::ANY, current_count) {
                return Some((z, RateSource::AnySlotBaseline));
            }
        }
        None
    }

    fn z_from_slot(
        &self,
        unit_key: u64,
        template: TemplateId,
        slot: Slot,
        current_count: u32,
    ) -> Option<f64> {
        let key = BaselineKey {
            unit_key,
            template_id: template,
            slot,
        };
        let hist = self.histograms.get(&key)?;
        if !hist.is_trusted(self.min_weight) {
            return None;
        }
        let (median, mad) = hist.median_mad()?;
        // Dieselbe poissonbewusste Untergrenze wie in Phase 3
        // (`RateTracker`): eine feste Untergrenze von 1.0 würde bei
        // typischen Raten von 2-3 Ereignissen pro Bucket schon gewöhnliches
        // Rauschen als 3-Sigma-Ereignis werten.
        let floor = (median + 1.0).sqrt().max(COUNT_MIN_SCALE);
        Some(z_score_from_median_mad(
            f64::from(current_count),
            median,
            mad,
            floor,
        ))
    }

    /// Surprisal eines Templates gegen das langfristige Unit-Profil, sofern
    /// dieses vertrauenswürdig ist. `None`, wenn kein Profil existiert oder
    /// dessen Beobachtungsgewicht die Schwelle nicht erreicht -- die
    /// Analyse-Engine fällt dann auf das Momentanfenster aus Phase 3 zurück.
    pub fn surprisal(&self, unit_key: u64, template: TemplateId, alpha: f64) -> Option<f64> {
        self.unit_profiles
            .get(&unit_key)
            .and_then(|profile| profile.surprisal(template, alpha, self.profile_min_weight))
    }

    /// Ob mindestens eine Baseline (Histogramm oder Unit-Profil) bereits
    /// vertrauenswürdig ist. Der Daemon nutzt dies, um die globale
    /// Lernphase nach dem Laden persistierter Baselines zu überspringen.
    pub fn has_trusted_baselines(&self) -> bool {
        self.histograms
            .values()
            .any(|hist| hist.is_trusted(self.min_weight))
            || self
                .unit_profiles
                .values()
                .any(|profile| profile.is_trusted(self.profile_min_weight))
    }

    /// Erzwingt `max_baselines`, indem so lange die Histogramme mit dem
    /// geringsten Beobachtungsgewicht verworfen werden, bis die Grenze
    /// eingehalten ist (Regel 18).
    ///
    /// `unit_profiles`/`unit_states` unterlagen bisher keiner eigenen
    /// Grenze: `record()` legt für jeden neu gesehenen `unit_key` einen
    /// Eintrag an, und nichts entfernte ihn je wieder. Auf einem Host mit
    /// transienten Unit-Namen (`session-<n>.scope` je Login,
    /// `docker-<hash>.scope`/`libpod-<hash>.scope` je Container,
    /// `run-u<n>.scope`) wächst das über die gesamte Laufzeit unbegrenzt --
    /// jeweils Verdrängung des am längsten inaktiven Eintrags, sobald auch
    /// diese Grenze erreicht ist.
    pub fn enforce_limits(&mut self) {
        while self.histograms.len() > self.max_baselines {
            let lightest = self
                .histograms
                .iter()
                .min_by(|(_, a), (_, b)| a.total_weight().total_cmp(&b.total_weight()))
                .map(|(key, _)| *key);
            match lightest {
                Some(key) => {
                    self.histograms.remove(&key);
                }
                None => break,
            }
        }

        while self.unit_profiles.len() > self.max_baselines {
            let oldest = self
                .unit_profiles
                .iter()
                .min_by_key(|(_, profile)| profile.last_touched_bucket())
                .map(|(key, _)| *key);
            match oldest {
                Some(key) => {
                    self.unit_profiles.remove(&key);
                }
                None => break,
            }
        }

        while self.unit_states.len() > self.max_baselines {
            let oldest = self
                .unit_states
                .iter()
                .min_by_key(|(_, state)| state.last_touched_bucket)
                .map(|(key, _)| *key);
            match oldest {
                Some(key) => {
                    self.unit_states.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Anzahl aktuell gehaltener Slot-Histogramme (für Diagnose/GUI).
    pub fn histogram_count(&self) -> usize {
        self.histograms.len()
    }

    /// Anzahl aktuell gehaltener Unit-Profile (für Diagnose/Tests).
    pub fn unit_profile_count(&self) -> usize {
        self.unit_profiles.len()
    }

    /// Anzahl aktuell gehaltener offener Bucket-Zustände je Unit (für
    /// Diagnose/Tests).
    pub fn unit_state_count(&self) -> usize {
        self.unit_states.len()
    }

    /// Zählwert des Templates im aktuell offenen (noch nicht
    /// abgeschlossenen) Bucket dieser Unit. 0, falls die Unit oder das
    /// Template darin noch nicht aktiv war. Wird von der Analyse-Engine
    /// genutzt, um [`BaselineStore::rate_z`] noch **vor** dem
    /// Bucket-Abschluss abzufragen -- exakt wie `RateTracker` in Phase 3
    /// den laufenden Bucket sofort auswertet, statt auf dessen Ende zu
    /// warten.
    pub fn current_bucket_count(&self, unit_key: u64, template: TemplateId) -> u32 {
        self.unit_states
            .get(&unit_key)
            .and_then(|state| state.bucket_counts.get(&template))
            .copied()
            .unwrap_or(0)
    }

    /// Serialisierbare Kopie des gesamten Baseline-Zustands. Der offene,
    /// noch nicht abgeschlossene Bucket-Zustand jeder Unit wird bewusst
    /// **nicht** mit gesichert: Er ist reine Laufzeit-Buchführung, sein
    /// Verlust beim Neustart kostet höchstens einen einzelnen Bucket an
    /// Beobachtung -- persistente Bucket-Zustände über einen Neustart
    /// hinweg würden dagegen eine erheblich komplexere Zeitrechnung
    /// erfordern, ohne einen Vorteil zu bieten, der diesen Aufwand
    /// rechtfertigt.
    pub fn snapshot(&self) -> BaselineSnapshot {
        BaselineSnapshot {
            histograms: self
                .histograms
                .iter()
                .map(|(key, hist)| (*key, hist.clone()))
                .collect(),
            profiles: self
                .unit_profiles
                .iter()
                .map(|(unit, profile)| (*unit, profile.clone()))
                .collect(),
        }
    }

    /// Baut einen Store aus einem zuvor erstellten Schnappschuss wieder auf.
    pub fn restore(
        snapshot: BaselineSnapshot,
        bucket_seconds: u64,
        config: &BaselineConfig,
    ) -> Self {
        let mut store = Self::new(bucket_seconds, config);
        store.histograms = snapshot.histograms.into_iter().collect();
        store.unit_profiles = snapshot.profiles.into_iter().collect();
        store
    }

    #[cfg(test)]
    pub(crate) fn test_histogram_weight(
        &self,
        unit_key: u64,
        template: TemplateId,
        slot: Slot,
    ) -> f64 {
        self.histograms
            .get(&BaselineKey {
                unit_key,
                template_id: template,
                slot,
            })
            .map_or(0.0, CountHistogram::total_weight)
    }

    #[cfg(test)]
    pub(crate) fn test_histogram_median_mad(
        &self,
        unit_key: u64,
        template: TemplateId,
        slot: Slot,
    ) -> Option<(f64, f64)> {
        self.histograms
            .get(&BaselineKey {
                unit_key,
                template_id: template,
                slot,
            })
            .and_then(CountHistogram::median_mad)
    }
}

/// Schließt den zuvor offenen Bucket `closing_bucket` einer Unit ab:
/// schreibt für jedes bekannte Template eine Beobachtung (den tatsächlichen
/// Zählwert oder 0, falls das Template in diesem Bucket nicht auftrat) in
/// dessen regulären Slot **und** zusätzlich in [`Slot::ANY`].
fn close_bucket(
    histograms: &mut HashMap<BaselineKey, CountHistogram>,
    unit_key: u64,
    state: &UnitBucketState,
    closing_bucket: u64,
    hist_config: &HistogramConfig,
) {
    for &template in state.known_templates.keys() {
        let count = state.bucket_counts.get(&template).copied().unwrap_or(0);
        let key = BaselineKey {
            unit_key,
            template_id: template,
            slot: state.open_slot,
        };
        observe_both_slots(histograms, key, count, closing_bucket, hist_config);
    }
}

/// Trägt eine Beobachtung sowohl in ihren regulären Slot als auch
/// zusätzlich in [`Slot::ANY`] ein. `key.slot` bestimmt den regulären Slot.
fn observe_both_slots(
    histograms: &mut HashMap<BaselineKey, CountHistogram>,
    key: BaselineKey,
    count: u32,
    bucket: u64,
    hist_config: &HistogramConfig,
) {
    histograms
        .entry(key)
        .or_default()
        .observe(count, bucket, hist_config);

    if key.slot != Slot::ANY {
        let any_key = BaselineKey {
            slot: Slot::ANY,
            ..key
        };
        histograms
            .entry(any_key)
            .or_default()
            .observe(count, bucket, hist_config);
    }
}

/// Verdrängt bei Bedarf das am längsten inaktive bekannte Template einer
/// Unit, damit `known_templates` nicht unbeschränkt wächst (Regel 18).
fn evict_known_if_needed(state: &mut UnitBucketState, max_known: usize) {
    let cap = max_known.max(1);
    if state.known_templates.len() <= cap {
        return;
    }
    if let Some((&oldest, _)) = state.known_templates.iter().min_by_key(|(_, &last)| last) {
        state.known_templates.remove(&oldest);
        state.bucket_counts.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fester Startzeitpunkt (2026-01-15T12:00:00Z), sodass Slot-Berechnungen
    /// deterministisch und -- solange innerhalb weniger Buckets getestet
    /// wird -- über die ganze Testdauer im selben Zeitprofil-Slot bleiben.
    const BASE_US: u64 = 1_768_478_400_000_000;
    const BUCKET_SECONDS: u64 = 5;
    const BUCKET_US: u64 = BUCKET_SECONDS * 1_000_000;

    fn tid(n: u64) -> TemplateId {
        TemplateId(n)
    }

    fn test_config() -> BaselineConfig {
        BaselineConfig {
            half_life_hours: 1_000_000.0, // im Test praktisch kein Zerfall
            min_weight: 3.0,
            profile_min_weight: 3.0,
            profile_max_templates: 100,
            max_bins: 32,
            max_baselines: 20_000,
        }
    }

    fn t(bucket: u64) -> u64 {
        BASE_US + bucket * BUCKET_US
    }

    #[test]
    fn frischer_store_hat_keine_vertrauenswuerdigen_baselines() {
        let store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        assert!(!store.has_trusted_baselines());
        assert_eq!(store.rate_z(t(0), 1, tid(1), 5), None);
        assert_eq!(store.surprisal(1, tid(1), 0.5), None);
    }

    #[test]
    fn ein_einzelnes_ereignis_committet_noch_nichts() {
        // Der Bucket bleibt offen, solange kein späteres Ereignis ihn
        // abschließt -- es darf also noch kein Histogramm-Eintrag existieren.
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        store.record(t(0), 1, tid(1));
        assert_eq!(store.histogram_count(), 0);
    }

    #[test]
    fn zweites_ereignis_in_neuem_bucket_committet_das_erste() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        store.record(t(0), 1, tid(1));
        store.record(t(1), 1, tid(1));
        let slot = Slot::from_timestamp_us(t(0));
        // Ein Eintrag im regulären Slot, einer in ANY.
        assert!((store.test_histogram_weight(1, tid(1), slot) - 1.0).abs() < 1e-6);
        assert!((store.test_histogram_weight(1, tid(1), Slot::ANY) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nullbeobachtung_nur_fuer_bekanntes_template_in_aktivem_bucket() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        let slot = Slot::from_timestamp_us(t(0));

        // Bucket 0: X und Y feuern gemeinsam.
        store.record(t(0), 1, tid(1)); // X
        store.record(t(0), 1, tid(2)); // Y
                                       // Bucket 5: nur Y feuert (aktiver Bucket, X bleibt stumm).
        store.record(t(5), 1, tid(2));
        // Bucket 10: erneut Y -> schließt Bucket 5 ab.
        store.record(t(10), 1, tid(2));

        // X: eine "1" aus Bucket 0, eine "0" aus Bucket 5 -> Gewicht 2.
        assert!(
            (store.test_histogram_weight(1, tid(1), slot) - 2.0).abs() < 1e-6,
            "X sollte in Bucket 5 eine Nullbeobachtung bekommen haben"
        );
        let (median_x, _) = store
            .test_histogram_median_mad(1, tid(1), slot)
            .expect("Histogramm vorhanden");
        assert!(
            (median_x - 0.5).abs() < 1e-6,
            "Median aus {{1,0}} sollte 0.5 sein, war {median_x}"
        );

        // Y: "1" aus Bucket 0, "1" aus Bucket 5 -> Gewicht 2, kein Nullwert.
        let (median_y, mad_y) = store
            .test_histogram_median_mad(1, tid(2), slot)
            .expect("Histogramm vorhanden");
        assert!(
            (median_y - 1.0).abs() < 1e-6,
            "Y feuerte immer, Median sollte 1 sein"
        );
        assert!((mad_y - 0.0).abs() < 1e-6);
    }

    #[test]
    fn stille_luecke_ohne_jede_aktivitaet_erzeugt_keine_nullbeobachtung() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        let slot = Slot::from_timestamp_us(t(0));

        store.record(t(0), 1, tid(1));
        // Riesige Lücke ohne jede Aktivität der Unit, dann wieder X.
        store.record(t(10_000), 1, tid(1));
        store.record(t(10_001), 1, tid(1));

        // Hätte die stille Lücke Nullen erzeugt, läge das Gewicht bei
        // tausenden; tatsächlich dürfen nur die beiden echten Buckets
        // (0 und 10000) gezählt haben.
        let weight = store.test_histogram_weight(1, tid(1), slot);
        assert!(
            weight <= 2.5,
            "stille Lücke darf keine Nullbeobachtungen erzeugt haben, Gewicht war {weight}"
        );
    }

    #[test]
    fn rate_z_nutzt_slot_baseline_wenn_vertrauenswuerdig() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        // Fünf Buckets mit stets genau 2 Ereignissen von X, jeweils im
        // selben Zeitprofil-Slot (kurzer Zeitraum).
        for bucket in 0..5u64 {
            store.record(t(bucket * 2), 1, tid(1));
            store.record(t(bucket * 2), 1, tid(1));
            // Nächster Bucket schließt den vorherigen ab.
            store.record(t(bucket * 2 + 1), 1, tid(99)); // Fülltemplate zum Schließen
        }

        let (z, source) = store
            .rate_z(t(20), 1, tid(1), 40)
            .expect("sollte trainiert sein");
        assert_eq!(source, RateSource::SlotBaseline);
        assert!(
            z > 3.0,
            "starker Ausschlag sollte hohen Z-Score liefern, war {z}"
        );
    }

    #[test]
    fn rate_z_faellt_auf_any_slot_zurueck_wenn_slot_selbst_nicht_traegt() {
        let cfg = BaselineConfig {
            min_weight: 5.0,
            ..test_config()
        };
        let mut store = BaselineStore::new(BUCKET_SECONDS, &cfg);

        // Zehn unterschiedliche Stunden, je genau eine Beobachtung: jeder
        // einzelne reguläre Slot bleibt bei Gewicht 1 (< 5, nicht
        // vertrauenswürdig), aber ANY sammelt alle zehn ein (>= 5).
        for hour in 0..10u64 {
            let base = BASE_US + hour * 3600 * 1_000_000;
            store.record(base, 1, tid(1));
            store.record(base + BUCKET_US, 1, tid(2)); // schließt ab
        }

        let query_time = BASE_US; // Stunde 0, per Konstruktion nur Gewicht 1.
        let (_, source) = store
            .rate_z(query_time, 1, tid(1), 5)
            .expect("ANY-Slot sollte tragen");
        assert_eq!(source, RateSource::AnySlotBaseline);
    }

    #[test]
    fn rate_z_ohne_jede_baseline_ist_none() {
        let store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        assert_eq!(store.rate_z(t(0), 1, tid(1), 5), None);
    }

    #[test]
    fn surprisal_konsistent_mit_unit_profile() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        for _ in 0..10 {
            store.record(t(0), 1, tid(1));
        }
        let s = store
            .surprisal(1, tid(1), 0.5)
            .expect("sollte trainiert sein");
        assert!(s.is_finite());
        assert!(s >= 0.0);
    }

    #[test]
    fn enforce_limits_haelt_die_obergrenze_ein() {
        let cfg = BaselineConfig {
            max_baselines: 6,
            ..test_config()
        };
        let mut store = BaselineStore::new(BUCKET_SECONDS, &cfg);

        // Zehn unabhängige Units, je zwei Ereignisse (öffnen + schließen),
        // ergibt 2 Histogramm-Einträge (regulär + ANY) je Unit -> 20 total.
        for unit in 0..10u64 {
            store.record(t(0), unit, tid(1));
            store.record(t(1), unit, tid(1));
        }
        assert!(store.histogram_count() > 6);

        store.enforce_limits();
        assert!(
            store.histogram_count() <= 6,
            "Obergrenze verletzt: {}",
            store.histogram_count()
        );
    }

    #[test]
    fn enforce_limits_deckelt_auch_unit_profile_und_unit_states() {
        // Regression: record() legt für jeden neuen unit_key permanent
        // einen Eintrag in unit_profiles UND unit_states an; ohne eigene
        // Verdrängung wuchsen beide unbegrenzt (z. B. bei transienten
        // Unit-Namen wie session-<n>.scope oder docker-<hash>.scope).
        let cfg = BaselineConfig {
            max_baselines: 6,
            ..test_config()
        };
        let mut store = BaselineStore::new(BUCKET_SECONDS, &cfg);

        for unit in 0..20u64 {
            store.record(t(unit), unit, tid(1));
        }
        assert!(store.unit_profile_count() > 6);
        assert!(store.unit_state_count() > 6);

        store.enforce_limits();
        assert!(
            store.unit_profile_count() <= 6,
            "unit_profiles-Obergrenze verletzt: {}",
            store.unit_profile_count()
        );
        assert!(
            store.unit_state_count() <= 6,
            "unit_states-Obergrenze verletzt: {}",
            store.unit_state_count()
        );
    }

    #[test]
    fn enforce_limits_bevorzugt_schwerere_eintraege() {
        let cfg = BaselineConfig {
            max_baselines: 4,
            ..test_config()
        };
        let mut store = BaselineStore::new(BUCKET_SECONDS, &cfg);
        let slot = Slot::from_timestamp_us(t(0));

        // Unit 1: viele Beobachtungen desselben Buckets/Templates -> hohes
        // Gewicht nach mehrfachem Schließen.
        for bucket in 0..10u64 {
            for _ in 0..5 {
                store.record(t(bucket * 2), 1, tid(1));
            }
            store.record(t(bucket * 2 + 1), 1, tid(99));
        }
        // Units 2..6: je nur ein einziges Commit -> Gewicht 1.
        for unit in 2..6u64 {
            store.record(t(0), unit, tid(1));
            store.record(t(1), unit, tid(1));
        }

        store.enforce_limits();

        assert!(
            store.test_histogram_weight(1, tid(1), slot) > 0.0,
            "das schwerste Histogramm darf nicht verworfen werden"
        );
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        for bucket in 0..5u64 {
            store.record(t(bucket * 2), 1, tid(1));
            store.record(t(bucket * 2), 1, tid(1));
            store.record(t(bucket * 2 + 1), 1, tid(99));
        }
        for _ in 0..10 {
            store.record(t(0), 1, tid(7));
        }

        let vorher_z = store.rate_z(t(20), 1, tid(1), 40);
        let vorher_surprisal = store.surprisal(1, tid(7), 0.5);
        let vorher_count = store.histogram_count();

        let snapshot = store.snapshot();
        let json = serde_json::to_string(&snapshot).expect("muss serialisierbar sein");
        let restored_snapshot: BaselineSnapshot =
            serde_json::from_str(&json).expect("muss deserialisierbar sein");

        let restored = BaselineStore::restore(restored_snapshot, BUCKET_SECONDS, &test_config());

        assert_eq!(restored.histogram_count(), vorher_count);
        assert_eq!(restored.rate_z(t(20), 1, tid(1), 40), vorher_z);
        assert_eq!(restored.surprisal(1, tid(7), 0.5), vorher_surprisal);
    }

    #[test]
    fn offener_bucket_zustand_wird_nicht_persistiert() {
        // Ein Ereignis ohne folgendes Ereignis committet nichts (siehe
        // `ein_einzelnes_ereignis_committet_noch_nichts`); der Snapshot
        // muss daher leer sein, nicht fehlerhaft irgendetwas enthalten.
        let mut store = BaselineStore::new(BUCKET_SECONDS, &test_config());
        store.record(t(0), 1, tid(1));
        let snapshot = store.snapshot();
        assert!(snapshot.histograms.is_empty());
    }
}
