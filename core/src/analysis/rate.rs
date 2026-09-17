//! Rate-Historie pro Template als Grundlage des robusten Z-Scores.
//!
//! Die Zeitachse wird in feste Buckets (z. B. 5 s) zerlegt. Pro Template
//! wird gezählt, wie viele Ereignisse in den aktuellen Bucket fallen; beim
//! Bucketwechsel wandert dieser Zählwert in eine beschränkte Historie. Der
//! Z-Score vergleicht dann den laufenden Bucket mit dieser Historie.
//!
//! Wichtig für die Statistik: Buckets, in denen ein Template *nicht*
//! auftrat, werden als Nullen in die Historie geschrieben. Ohne diese
//! Nullen bestünde die Historie eines seltenen Templates nur aus seinen
//! Auftritten, und der Median läge fälschlich bei dessen typischer
//! Burst-Größe statt bei 0.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::template::TemplateId;

use super::stats::{count_scale_floor, robust_z_score};

/// Zustand eines einzelnen Templates in der Rate-Verfolgung.
#[derive(Debug)]
struct TemplateRate {
    /// Zählwert im aktuell laufenden Bucket.
    current_count: u64,
    /// Beschränkte Historie abgeschlossener Buckets.
    history: VecDeque<f64>,
    /// Nummer des Buckets, in dem dieses Template zuletzt auftrat.
    last_active_bucket: u64,
}

/// Verfolgt Ereignisraten pro Template über gleich lange Zeit-Buckets.
pub struct RateTracker {
    templates: HashMap<TemplateId, TemplateRate>,
    bucket_us: u64,
    history_len: usize,
    max_templates: usize,
    /// Nach wie vielen inaktiven Buckets ein Template vergessen wird
    /// (Regel 18: der Speicher darf nicht unbegrenzt wachsen).
    idle_eviction_buckets: u64,
    /// Nummer des aktuell laufenden Buckets.
    current_bucket: u64,
    /// Bucket, in dem die Beobachtung begann. Begrenzt, wie viel
    /// Null-Vorgeschichte einem neuen Template zugeschrieben werden darf.
    start_bucket: u64,
    /// Ob bereits ein erstes Ereignis verarbeitet wurde.
    started: bool,
}

impl RateTracker {
    /// Erstellt eine neue Rate-Verfolgung.
    pub fn new(
        bucket_seconds: u64,
        history_len: usize,
        max_templates: usize,
        idle_eviction_buckets: u64,
    ) -> Self {
        Self {
            templates: HashMap::new(),
            bucket_us: bucket_seconds.max(1).saturating_mul(1_000_000),
            history_len: history_len.max(1),
            max_templates: max_templates.max(1),
            idle_eviction_buckets: idle_eviction_buckets.max(1),
            current_bucket: 0,
            start_bucket: 0,
            started: false,
        }
    }

    /// Bucketnummer zu einem Zeitstempel.
    fn bucket_of(&self, timestamp_us: u64) -> u64 {
        timestamp_us / self.bucket_us
    }

    /// Verarbeitet ein Ereignis und liefert den robusten Z-Score des
    /// Templates im laufenden Bucket gegenüber seiner Historie.
    ///
    /// Der Aufruf schließt implizit alle Buckets ab, die seit dem letzten
    /// Ereignis vergangen sind.
    pub fn record(&mut self, timestamp_us: u64, template_id: TemplateId) -> f64 {
        let bucket = self.bucket_of(timestamp_us);

        if !self.started {
            self.current_bucket = bucket;
            self.start_bucket = bucket;
            self.started = true;
        } else if bucket > self.current_bucket {
            self.roll_over_to(bucket);
        }

        // Verspätete Ereignisse aus einem bereits abgeschlossenen Bucket
        // werden dem laufenden Bucket zugeschlagen, statt sie zu verwerfen.
        self.ensure_capacity();

        // Ein erstmals gesehenes Template bekommt eine Historie aus Nullen
        // für die Zeit, die seit Beobachtungsbeginn vergangen ist. Das ist
        // die sachlich richtige Vorgeschichte: Vor der Erstsichtung trat es
        // schlicht nicht auf. Ohne diese Vorbefüllung wäre die Historie
        // leer, der erste abgeschlossene Bucket würde zum Median -- und ein
        // Log-Sturm eines neuen Templates erklärte sich damit binnen eines
        // Buckets selbst zur Normalität und verschwände aus der Erkennung.
        let prefill = (self.current_bucket.saturating_sub(self.start_bucket))
            .min(self.history_len as u64) as usize;
        let history_len = self.history_len;
        let current_bucket = self.current_bucket;

        let entry = self
            .templates
            .entry(template_id)
            .or_insert_with(|| TemplateRate {
                current_count: 0,
                history: std::iter::repeat_n(0.0, prefill).collect(),
                last_active_bucket: current_bucket,
            });
        debug_assert!(entry.history.len() <= history_len);
        entry.current_count += 1;
        entry.last_active_bucket = self.current_bucket;

        let history: Vec<f64> = entry.history.iter().copied().collect();
        let current = entry.current_count as f64;
        let floor = count_scale_floor(&history);
        robust_z_score(current, &history, floor)
    }

    /// Schließt alle Buckets bis einschließlich `target - 1` ab und setzt
    /// den laufenden Bucket auf `target`.
    fn roll_over_to(&mut self, target: u64) {
        let gap = target.saturating_sub(self.current_bucket);
        // Mehr Nullen als die Historie fasst, würden diese ohnehin komplett
        // überschreiben; die Deckelung hält lange Ruhephasen billig.
        let fill = gap.min(self.history_len as u64);

        let history_len = self.history_len;
        // Die Grenze muss sich am *Ziel*-Bucket orientieren, nicht am alten
        // laufenden Bucket: sonst wächst bei einem großen Zeitsprung die
        // Grenze nicht mit, und lange inaktive Templates verfallen nie.
        let cutoff = target.saturating_sub(self.idle_eviction_buckets);

        self.templates.retain(|_, rate| {
            // Abgeschlossenen Zählwert übernehmen, danach die übersprungenen
            // Buckets als Nullen auffüllen.
            push_bounded(&mut rate.history, rate.current_count as f64, history_len);
            for _ in 1..fill {
                push_bounded(&mut rate.history, 0.0, history_len);
            }
            rate.current_count = 0;

            // Lange inaktive Templates vergessen.
            rate.last_active_bucket >= cutoff
        });

        self.current_bucket = target;
    }

    /// Stellt sicher, dass die Kapazitätsgrenze nicht überschritten wird,
    /// indem im Bedarfsfall das am längsten inaktive Template entfernt wird.
    fn ensure_capacity(&mut self) {
        if self.templates.len() < self.max_templates {
            return;
        }
        let oldest = self
            .templates
            .iter()
            .min_by_key(|(_, rate)| rate.last_active_bucket)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.templates.remove(&id);
        }
    }

    /// Anzahl aktuell verfolgter Templates.
    pub fn tracked_templates(&self) -> usize {
        self.templates.len()
    }

    /// Länge der Historie eines Templates (für Tests und Diagnose).
    pub fn history_len_of(&self, template_id: TemplateId) -> usize {
        self.templates
            .get(&template_id)
            .map_or(0, |rate| rate.history.len())
    }
}

/// Fügt einen Wert an und hält dabei die Obergrenze ein.
fn push_bounded(history: &mut VecDeque<f64>, value: f64, max_len: usize) {
    if history.len() >= max_len {
        history.pop_front();
    }
    history.push_back(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000;

    fn tid(n: u64) -> TemplateId {
        TemplateId(n)
    }

    #[test]
    fn erster_eintrag_ohne_historie_ergibt_z_score_null() {
        let mut tracker = RateTracker::new(5, 60, 100, 1000);
        let z = tracker.record(0, tid(1));
        assert!(
            z.abs() < 1e-9,
            "ohne Historie darf kein Alarm entstehen, war {z}"
        );
    }

    #[test]
    fn historie_waechst_bei_bucketwechsel() {
        let mut tracker = RateTracker::new(5, 60, 100, 1000);
        tracker.record(0, tid(1));
        assert_eq!(tracker.history_len_of(tid(1)), 0);
        // Nächster Bucket -> der abgeschlossene Zählwert wandert in die Historie.
        tracker.record(6 * SEC, tid(1));
        assert_eq!(tracker.history_len_of(tid(1)), 1);
    }

    #[test]
    fn historie_ist_beschraenkt() {
        let mut tracker = RateTracker::new(1, 5, 100, 10_000);
        for i in 0..100 {
            tracker.record(i * SEC, tid(1));
        }
        assert_eq!(
            tracker.history_len_of(tid(1)),
            5,
            "Historie darf die konfigurierte Länge nicht überschreiten"
        );
    }

    #[test]
    fn burst_gegen_ruhige_historie_ergibt_hohen_z_score() {
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        // 30 Buckets mit je genau einem Ereignis.
        for i in 0..30 {
            tracker.record(i * SEC, tid(1));
        }
        // Jetzt ein Burst von 50 Ereignissen im selben Bucket.
        let mut last_z = 0.0;
        for _ in 0..50 {
            last_z = tracker.record(30 * SEC, tid(1));
        }
        assert!(
            last_z > 5.0,
            "Burst sollte deutlichen Z-Score liefern, war {last_z}"
        );
    }

    #[test]
    fn gleichmaessige_rate_erzeugt_keinen_hohen_z_score() {
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        let mut last_z = 0.0;
        for i in 0..60 {
            // Konstant zwei Ereignisse pro Bucket.
            tracker.record(i * SEC, tid(1));
            last_z = tracker.record(i * SEC, tid(1));
        }
        assert!(
            last_z.abs() < 3.0,
            "konstante Rate darf keinen Alarm auslösen, war {last_z}"
        );
    }

    #[test]
    fn ruhephasen_werden_als_nullen_in_die_historie_geschrieben() {
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        tracker.record(0, tid(1));
        // Große Lücke: viele leere Buckets.
        tracker.record(20 * SEC, tid(1));
        assert!(
            tracker.history_len_of(tid(1)) > 1,
            "übersprungene Buckets müssen als Nullen erscheinen"
        );
    }

    #[test]
    fn seltenes_template_mit_burst_wird_erkannt() {
        // Ein Template, das normalerweise gar nicht auftritt, dann plötzlich
        // 20-mal in einem Bucket. Die Nullen-Historie ist hier entscheidend.
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        tracker.record(0, tid(9));
        for i in 1..40 {
            // Anderes Template hält die Uhr am Laufen.
            tracker.record(i * SEC, tid(1));
        }
        let mut last_z = 0.0;
        for _ in 0..20 {
            last_z = tracker.record(40 * SEC, tid(9));
        }
        assert!(
            last_z > 5.0,
            "Burst eines sonst stillen Templates sollte auffallen, war {last_z}"
        );
    }

    #[test]
    fn neues_template_erhaelt_null_vorgeschichte() {
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        // 30 Buckets lang läuft nur ein anderes Template.
        for i in 0..30 {
            tracker.record(i * SEC, tid(1));
        }
        // Jetzt taucht ein neues Template erstmals auf: seine Vorgeschichte
        // muss aus Nullen bestehen, nicht leer sein.
        tracker.record(30 * SEC, tid(9));
        assert_eq!(
            tracker.history_len_of(tid(9)),
            30,
            "die Zeit vor der Erstsichtung muss als Nullen erscheinen"
        );
    }

    #[test]
    fn log_sturm_eines_neuen_templates_erklaert_sich_nicht_selbst_zur_norm() {
        let mut tracker = RateTracker::new(5, 60, 100, 10_000);
        // Vorlauf, damit überhaupt eine Vorgeschichte existiert.
        for i in 0..60 {
            tracker.record(i * SEC, tid(1));
        }
        // Neues Template stürmt los und hält die Rate über mehrere Buckets.
        let mut z_werte = Vec::new();
        for bucket in 0..5u64 {
            let mut last = 0.0;
            for i in 0..100u64 {
                last = tracker.record(60 * SEC + bucket * 5 * SEC + i * 10_000, tid(9));
            }
            z_werte.push(last);
        }
        // Ohne Null-Vorgeschichte fiele der Z-Score ab dem zweiten Bucket
        // auf 0, weil der eigene Burst zum Median würde.
        for (bucket, z) in z_werte.iter().enumerate() {
            assert!(
                *z > 5.0,
                "anhaltender Sturm muss in Bucket {bucket} auffällig bleiben, war {z}"
            );
        }
    }

    #[test]
    fn beobachtungsbeginn_begrenzt_die_null_vorgeschichte() {
        let mut tracker = RateTracker::new(1, 60, 100, 10_000);
        // Direkt zu Beginn: es gibt noch keine Vorgeschichte, die man einem
        // neuen Template zuschreiben könnte.
        let z = tracker.record(0, tid(1));
        assert_eq!(tracker.history_len_of(tid(1)), 0);
        assert!(
            z.abs() < 1e-9,
            "am Beobachtungsbeginn darf kein Alarm entstehen"
        );
    }

    #[test]
    fn inaktive_templates_werden_vergessen() {
        let mut tracker = RateTracker::new(1, 10, 100, 5);
        tracker.record(0, tid(1));
        assert_eq!(tracker.tracked_templates(), 1);
        // Weit in der Zukunft mit einem anderen Template weitermachen.
        tracker.record(100 * SEC, tid(2));
        assert_eq!(
            tracker.tracked_templates(),
            1,
            "das lange inaktive Template sollte entfernt sein"
        );
    }

    #[test]
    fn kapazitaetsgrenze_wird_eingehalten() {
        let mut tracker = RateTracker::new(1, 10, 5, 100_000);
        for i in 0..50 {
            tracker.record(i * SEC, tid(i));
        }
        assert!(
            tracker.tracked_templates() <= 5,
            "Kapazitätsgrenze verletzt: {}",
            tracker.tracked_templates()
        );
    }
}
