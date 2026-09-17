//! Gewichtetes Histogramm von Bucket-Zählwerten je Baseline.
//!
//! Siehe `docs/phase4-baselines.md` Abschnitt 3.1 für die Abwägung gegen
//! einen Ring der letzten N Werte und einen P²-Quantilschätzer: ein
//! Histogramm ist hier sowohl exakt (Median/MAD ohne Näherung) als auch
//! kompakt, weil Bucket-Zählwerte kleine Ganzzahlen mit wenigen
//! unterschiedlichen Ausprägungen je Slot sind.
//!
//! Abweichung vom Signatur-Vertrag im Entwurfsdokument: `observe` erhält
//! statt einzelner `decay: &DecayParams` ein gebündeltes
//! [`HistogramConfig`] (Zerfall + Bin-Obergrenze), da beide Parameter bei
//! jedem Aufruf gemeinsam gebraucht werden und die Signatur sonst mit jeder
//! künftig hinzukommenden Grenze weiterwachsen würde.

use serde::{Deserialize, Serialize};

use super::decay::DecayParams;

/// Bündelt die für [`CountHistogram::observe`] nötigen Laufzeitparameter.
#[derive(Debug, Clone, Copy)]
pub struct HistogramConfig {
    /// Zerfallsparameter (Halbwertszeit in Bucket-Einheiten).
    pub decay: DecayParams,
    /// Harte Obergrenze unterschiedlicher Zählwerte je Histogramm
    /// (Regel 18). Darüber hinaus wird der Bin mit dem höchsten Zählwert
    /// zum Sammelbin für alle weiteren, bisher ungesehenen Zählwerte.
    pub max_bins: usize,
}

/// Gewichtetes Histogramm: Zählwert (Ereignisse in einem Bucket) →
/// akkumuliertes, zerfallendes Gewicht.
///
/// `f32` für die Gewichte reicht: Sie sind bereits das Ergebnis
/// exponentiellen Zerfalls, absolute Präzision jenseits weniger
/// Nachkommastellen hat hier keine Aussagekraft, und die Platzersparnis
/// wirkt sich bei potenziell tausenden Histogrammen (Regel: `max_baselines`)
/// spürbar auf den RSS des Daemons aus.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct CountHistogram {
    /// Nicht notwendigerweise sortiert; `observe`/`median_mad` sortieren
    /// bei Bedarf. Reihenfolge bleibt für gleiche Eingaben aber stabil,
    /// solange sich Bins nicht durch Verdrängung ändern.
    bins: Vec<(u32, f32)>,
    last_touched_bucket: u64,
}

/// Gewichte unterhalb dieser Schwelle gelten als vernachlässigbar und
/// werden entfernt, damit Leichen einmaliger Ausreißer nicht ewig als Bins
/// liegen bleiben (siehe `docs/phase4-baselines.md` Abschnitt 3.2).
const NEGLIGIBLE_WEIGHT: f32 = 1e-3;

impl CountHistogram {
    /// Nimmt eine Beobachtung auf: wendet zuerst den Zerfall seit dem
    /// letzten Zugriff an, trägt dann den neuen Zählwert ein.
    pub fn observe(&mut self, count: u32, bucket: u64, config: &HistogramConfig) {
        self.apply_decay(bucket, &config.decay);
        self.insert(count, config.max_bins.max(1));
        self.last_touched_bucket = self.last_touched_bucket.max(bucket);
        self.prune_negligible();
    }

    /// Wendet den Zerfall für die seit `last_touched_bucket` vergangenen
    /// Buckets an. Verspätete Ereignisse (kleinerer oder gleicher Bucket)
    /// lösen keinen (negativen) Zerfall aus.
    fn apply_decay(&mut self, bucket: u64, decay: &DecayParams) {
        let elapsed = bucket.saturating_sub(self.last_touched_bucket);
        if elapsed == 0 {
            return;
        }
        let factor = decay.factor(elapsed) as f32;
        for (_, weight) in &mut self.bins {
            *weight *= factor;
        }
    }

    /// Trägt einen Zählwert ein: erhöht einen bestehenden Bin, legt einen
    /// neuen an, oder verdrängt in den Sammelbin, wenn `max_bins` erreicht
    /// ist.
    fn insert(&mut self, count: u32, max_bins: usize) {
        if let Some(entry) = self.bins.iter_mut().find(|(c, _)| *c == count) {
            entry.1 += 1.0;
            return;
        }
        if self.bins.len() < max_bins {
            self.bins.push((count, 1.0));
            return;
        }
        // Sammelbin: der Bin mit dem höchsten bisherigen Zählwert nimmt
        // jede weitere, bisher ungesehene Ausprägung auf. Für Median/MAD
        // ist das unschädlich, solange der Median klar darunter liegt --
        // und ein Median jenseits von `max_bins` unterschiedlichen
        // Zählwerten ist selbst schon eine Rate, bei der die absolute
        // Skala nicht mehr entscheidend ist (siehe Entwurfsdokument).
        if let Some(collector) = self.bins.iter_mut().max_by_key(|(c, _)| *c) {
            collector.1 += 1.0;
        }
    }

    fn prune_negligible(&mut self) {
        self.bins.retain(|(_, weight)| *weight >= NEGLIGIBLE_WEIGHT);
    }

    /// Summe aller Bin-Gewichte: das Beobachtungsgewicht dieser Baseline.
    pub fn total_weight(&self) -> f64 {
        self.bins.iter().map(|(_, weight)| f64::from(*weight)).sum()
    }

    /// Gewichteter Median und gewichtete mittlere absolute Abweichung
    /// (MAD) über die im Histogramm gehaltenen Zählwerte.
    ///
    /// `None`, wenn das Histogramm leer ist (Gesamtgewicht 0).
    pub fn median_mad(&self) -> Option<(f64, f64)> {
        if self.bins.is_empty() {
            return None;
        }
        let pairs: Vec<(f64, f64)> = self
            .bins
            .iter()
            .map(|(count, weight)| (f64::from(*count), f64::from(*weight)))
            .collect();
        let median = weighted_median(&pairs)?;

        let deviations: Vec<(f64, f64)> = pairs
            .iter()
            .map(|(value, weight)| ((value - median).abs(), *weight))
            .collect();
        let mad = weighted_median(&deviations)?;

        Some((median, mad))
    }

    /// Ob das Beobachtungsgewicht die Vertrauensschwelle erreicht.
    pub fn is_trusted(&self, min_weight: f64) -> bool {
        self.total_weight() >= min_weight
    }

    /// Zeitpunkt (als Bucket-Nummer) der letzten Beobachtung.
    pub fn last_touched_bucket(&self) -> u64 {
        self.last_touched_bucket
    }
}

/// Gewichteter Median einer Menge von (Wert, Gewicht)-Paaren.
///
/// Definiert als der kleinste Wert, bei dem die kumulierte Gewichtssumme
/// (in aufsteigender Wert-Reihenfolge) mindestens die Hälfte des
/// Gesamtgewichts erreicht. Liegt die kumulierte Summe an dieser Stelle
/// exakt bei der Hälfte und existiert ein nächstgrößerer Wert, wird der
/// Mittelwert beider Werte zurückgegeben -- das entspricht der üblichen
/// Konvention bei einer geraden Anzahl unwerteter Beobachtungen.
///
/// `None` bei Gesamtgewicht 0.
fn weighted_median(pairs: &[(f64, f64)]) -> Option<f64> {
    let total: f64 = pairs.iter().map(|(_, weight)| weight).sum();
    if total <= 0.0 {
        return None;
    }

    let mut sorted: Vec<(f64, f64)> = pairs.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));

    let half = total / 2.0;
    let mut cumulative = 0.0;
    for (i, (value, weight)) in sorted.iter().enumerate() {
        cumulative += weight;
        if cumulative >= half {
            if (cumulative - half).abs() < 1e-9 && i + 1 < sorted.len() {
                return Some((value + sorted[i + 1].0) / 2.0);
            }
            return Some(*value);
        }
    }
    sorted.last().map(|(value, _)| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(half_life_buckets: f64, max_bins: usize) -> HistogramConfig {
        HistogramConfig {
            decay: DecayParams::from_buckets(half_life_buckets),
            max_bins,
        }
    }

    #[test]
    fn leeres_histogramm_hat_kein_median_mad() {
        let hist = CountHistogram::default();
        assert_eq!(hist.median_mad(), None);
        assert_eq!(hist.total_weight(), 0.0);
        assert!(!hist.is_trusted(0.0001));
    }

    #[test]
    fn median_und_mad_gegen_bekannte_werte() {
        // Klassische Referenz: [1,1,1,1,1,2,3] -> Median 1, MAD 0
        // (Abweichungen [0,0,0,0,0,1,2], deren Median ist 0).
        let mut hist = CountHistogram::default();
        let cfg = config(1000.0, 32);
        for &v in &[1, 1, 1, 1, 1, 2, 3] {
            hist.observe(v, 0, &cfg);
        }
        let (median, mad) = hist.median_mad().expect("nicht leer");
        assert!((median - 1.0).abs() < 1e-9, "Median war {median}");
        assert!((mad - 0.0).abs() < 1e-9, "MAD war {mad}");
    }

    #[test]
    fn median_bei_gerader_gewichtssumme_mittelt() {
        // [2, 4] gleich gewichtet -> Median (2+4)/2 = 3.
        let mut hist = CountHistogram::default();
        let cfg = config(1000.0, 32);
        hist.observe(2, 0, &cfg);
        hist.observe(4, 0, &cfg);
        let (median, _) = hist.median_mad().expect("nicht leer");
        assert!((median - 3.0).abs() < 1e-9, "Median war {median}");
    }

    #[test]
    fn gewicht_steigt_mit_jeder_beobachtung() {
        let mut hist = CountHistogram::default();
        let cfg = config(1000.0, 32);
        hist.observe(1, 0, &cfg);
        assert!((hist.total_weight() - 1.0).abs() < 1e-6);
        hist.observe(1, 0, &cfg);
        assert!((hist.total_weight() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn zerfall_halbiert_gewicht_nach_einer_halbwertszeit() {
        let mut hist = CountHistogram::default();
        let cfg = config(10.0, 32);
        hist.observe(5, 0, &cfg);
        assert!((hist.total_weight() - 1.0).abs() < 1e-6);
        // 10 Buckets später: eine Halbwertszeit vergangen.
        hist.observe(5, 10, &cfg);
        // Altes Gewicht (1.0) zerfällt auf 0.5, plus neue Beobachtung (1.0).
        assert!(
            (hist.total_weight() - 1.5).abs() < 1e-6,
            "Gesamtgewicht war {}",
            hist.total_weight()
        );
    }

    #[test]
    fn verspaetete_beobachtung_loest_keinen_negativen_zerfall_aus() {
        let mut hist = CountHistogram::default();
        let cfg = config(10.0, 32);
        hist.observe(5, 100, &cfg);
        let gewicht_vorher = hist.total_weight();
        // Ein Ereignis aus einem früheren Bucket darf das Gewicht nicht
        // hochrechnen (kein negativer Zerfall).
        hist.observe(5, 50, &cfg);
        assert!(hist.total_weight() >= gewicht_vorher);
        assert_eq!(hist.last_touched_bucket(), 100);
    }

    #[test]
    fn winzige_gewichte_werden_nach_starkem_zerfall_entfernt() {
        let mut hist = CountHistogram::default();
        let cfg = config(1.0, 32);
        hist.observe(7, 0, &cfg);
        // Sehr viele Halbwertszeiten später: das Gewicht unterschreitet die
        // Vernachlässigbarkeitsschwelle und muss verschwinden.
        hist.observe(9, 1000, &cfg);
        assert!(
            hist.median_mad()
                .is_some_and(|(m, _)| (m - 9.0).abs() < 1e-9),
            "die uralte Beobachtung darf den Median nicht mehr verfälschen"
        );
    }

    #[test]
    fn sammelbin_erzwingt_die_max_bins_obergrenze() {
        let mut hist = CountHistogram::default();
        let cfg = config(1000.0, 3);
        for v in [1, 2, 3, 4, 5] {
            hist.observe(v, 0, &cfg);
        }
        assert!(
            hist.bins.len() <= 3,
            "Obergrenze verletzt: {} Bins",
            hist.bins.len()
        );
        // Kein Gewicht darf durch die Verdrängung verloren gehen.
        assert!((hist.total_weight() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn vertrauensschwelle_greift_erst_ab_ausreichendem_gewicht() {
        let mut hist = CountHistogram::default();
        let cfg = config(1000.0, 32);
        for _ in 0..5 {
            hist.observe(2, 0, &cfg);
        }
        assert!(!hist.is_trusted(10.0));
        assert!(hist.is_trusted(5.0));
    }

    #[test]
    fn weighted_median_stimmt_mit_unklassiertem_median_ueberein() {
        // Jeder Wert einzeln mit Gewicht 1 -- muss dem klassischen,
        // ungewichteten Median entsprechen.
        let werte = [7.0, 2.0, 9.0, 4.0, 4.0];
        let pairs: Vec<(f64, f64)> = werte.iter().map(|v| (*v, 1.0)).collect();
        let median = weighted_median(&pairs).expect("nicht leer");
        // Sortiert: [2,4,4,7,9] -> Median 4.
        assert!((median - 4.0).abs() < 1e-9, "war {median}");
    }

    #[test]
    fn weighted_median_leerer_eingabe_ist_none() {
        assert_eq!(weighted_median(&[]), None);
    }
}
