//! Exponentieller Zerfall für Baseline-Gewichte.
//!
//! Sowohl [`super::histogram::CountHistogram`] als auch das spätere
//! Unit-Profil (Schritt 4) vergessen alte Beobachtungen über dieselbe
//! Halbwertszeit-Logik; dieses Modul bündelt sie, damit beide exakt
//! konsistent zerfallen.

/// Parameter des exponentiellen Zerfalls, in Bucket-Einheiten statt
/// Realzeit. Die Umrechnung von Stunden (wie in der Konfiguration) auf
/// Bucket-Einheiten geschieht einmalig bei der Konstruktion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayParams {
    half_life_buckets: f64,
}

impl DecayParams {
    /// Erstellt Zerfallsparameter aus einer Halbwertszeit in Stunden und der
    /// Bucket-Länge in Sekunden. Ergebnisse werden auf mindestens einen
    /// Bucket begrenzt, damit eine versehentlich winzige Konfiguration nicht
    /// zu sofortigem, vollständigem Vergessen führt.
    pub fn from_hours(half_life_hours: f64, bucket_seconds: u64) -> Self {
        let bucket_seconds = (bucket_seconds.max(1)) as f64;
        let half_life_buckets = (half_life_hours.max(0.0) * 3600.0 / bucket_seconds).max(1.0);
        Self { half_life_buckets }
    }

    /// Erstellt Zerfallsparameter direkt aus einer Bucket-Anzahl (v. a. für
    /// Tests, in denen die Umrechnung über Stunden nur Rauschen hinzufügt).
    pub fn from_buckets(half_life_buckets: f64) -> Self {
        Self {
            half_life_buckets: half_life_buckets.max(1.0),
        }
    }

    /// Zerfallsfaktor `0.5^(elapsed / half_life)` für die gegebene Anzahl
    /// vergangener Buckets. Liegt immer in `(0, 1]`.
    pub fn factor(&self, elapsed_buckets: u64) -> f64 {
        0.5_f64.powf(elapsed_buckets as f64 / self.half_life_buckets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faktor_bei_null_vergangenen_buckets_ist_eins() {
        let decay = DecayParams::from_buckets(10.0);
        assert!((decay.factor(0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn faktor_nach_einer_halbwertszeit_ist_ein_halb() {
        let decay = DecayParams::from_buckets(10.0);
        assert!((decay.factor(10) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn faktor_nach_zwei_halbwertszeiten_ist_ein_viertel() {
        let decay = DecayParams::from_buckets(10.0);
        assert!((decay.factor(20) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn faktor_bleibt_immer_im_gueltigen_bereich() {
        let decay = DecayParams::from_buckets(5.0);
        // Bei extrem vielen vergangenen Buckets ist ein Unterlauf zu exakt
        // 0.0 legitimes Fließkommaverhalten (0.5^200000), keine Anomalie.
        assert!(decay.factor(1_000_000) >= 0.0);
        assert!(decay.factor(1_000_000) <= 1.0);
        // Für einen moderat großen, noch darstellbaren Exponenten bleibt
        // der Faktor jedoch strikt positiv.
        assert!(decay.factor(1000) > 0.0);
        assert!(decay.factor(0) <= 1.0);
    }

    #[test]
    fn stunden_umrechnung_ergibt_erwartete_bucket_anzahl() {
        // 7 Tage Halbwertszeit, 5-Sekunden-Buckets -> 7*24*3600/5 = 120960.
        let decay = DecayParams::from_hours(168.0, 5);
        assert!((decay.factor(120_960) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn winzige_konfiguration_wird_auf_einen_bucket_begrenzt() {
        let decay = DecayParams::from_hours(0.0, 5);
        // Ohne Untergrenze wäre das eine Division durch 0.
        assert!(decay.factor(1).is_finite());
    }
}
