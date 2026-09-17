//! Langfristiges Häufigkeitsprofil der Templates einer Unit.
//!
//! Liefert den Nenner für das Surprisal (`docs/phase4-baselines.md`
//! Abschnitt 5): Statt gegen das 60-Sekunden-Momentanfenster (Phase 3) wird
//! ein neues Ereignis gegen seine Häufigkeit über Tage hinweg bewertet.
//! Das ist die gezielte Antwort auf die in Phase 3 dokumentierte
//! sturmkorrelierte Fehlalarmquelle: Ein einminütiger Bruteforce-Sturm
//! verändert ein mit sieben Tagen Halbwertszeit gewichtetes Profil nur
//! marginal, während er das Momentanfenster vollständig dominiert.
//!
//! Struktur und Zerfall folgen bewusst demselben Muster wie
//! [`super::histogram::CountHistogram`] (dieselbe [`DecayParams`]-Logik),
//! sind hier aber nach `TemplateId` statt nach Zählwert gebucket -- eine
//! gemeinsame Abstraktion wurde verworfen, weil die Verdrängungspolitik
//! bei Kapazitätsüberschreitung unterschiedlich ist (Sammelbin beim
//! Histogramm, Verwerfen des leichtesten Eintrags hier; siehe
//! [`UnitProfile::evict_lightest_if_needed`]).

use serde::{Deserialize, Serialize};

use crate::template::TemplateId;

use super::decay::DecayParams;

/// Bündelt die für [`UnitProfile::observe`] nötigen Laufzeitparameter.
#[derive(Debug, Clone, Copy)]
pub struct ProfileConfig {
    /// Zerfallsparameter (typischerweise mit deutlich längerer Halbwertszeit
    /// als das Histogramm der Rate-Baselines -- das Profil soll gerade
    /// *nicht* auf kurzfristige Stürme reagieren).
    pub decay: DecayParams,
    /// Harte Obergrenze unterschiedlicher Templates je Profil (Regel 18).
    pub max_templates: usize,
}

/// Langfristige, zerfallende Häufigkeitsverteilung der Templates einer Unit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct UnitProfile {
    counts: Vec<(TemplateId, f32)>,
    last_touched_bucket: u64,
}

/// Dieselbe Vernachlässigbarkeitsschwelle wie im Histogramm
/// (`super::histogram`), damit beide Baseline-Arten konsistent vergessen.
const NEGLIGIBLE_WEIGHT: f32 = 1e-3;

impl UnitProfile {
    /// Nimmt eine Beobachtung auf: Zerfall seit dem letzten Zugriff, dann
    /// Eintrag des Templates.
    pub fn observe(&mut self, template: TemplateId, bucket: u64, config: &ProfileConfig) {
        self.apply_decay(bucket, &config.decay);
        self.insert(template, config.max_templates.max(1));
        self.last_touched_bucket = self.last_touched_bucket.max(bucket);
        self.prune_negligible();
    }

    fn apply_decay(&mut self, bucket: u64, decay: &DecayParams) {
        let elapsed = bucket.saturating_sub(self.last_touched_bucket);
        if elapsed == 0 {
            return;
        }
        let factor = decay.factor(elapsed) as f32;
        for (_, weight) in &mut self.counts {
            *weight *= factor;
        }
    }

    fn insert(&mut self, template: TemplateId, max_templates: usize) {
        if let Some(entry) = self.counts.iter_mut().find(|(t, _)| *t == template) {
            entry.1 += 1.0;
            return;
        }
        self.evict_lightest_if_needed(max_templates);
        self.counts.push((template, 1.0));
    }

    /// Verwirft bei Bedarf den Eintrag mit dem geringsten Gewicht, bevor ein
    /// neuer hinzukommt. Anders als beim Histogramm (Sammelbin) gibt es
    /// hier keine sinnvolle Zusammenfassung fremder Templates: Ihre Identität
    /// *ist* die Information. Das am wenigsten gewichtete, also am längsten
    /// nicht mehr gestärkte Template zu opfern ist der verträglichste Verlust.
    fn evict_lightest_if_needed(&mut self, max_templates: usize) {
        if self.counts.len() < max_templates {
            return;
        }
        if let Some((idx, _)) = self
            .counts
            .iter()
            .enumerate()
            .min_by(|(_, (_, a)), (_, (_, b))| a.total_cmp(b))
        {
            self.counts.remove(idx);
        }
    }

    fn prune_negligible(&mut self) {
        self.counts
            .retain(|(_, weight)| *weight >= NEGLIGIBLE_WEIGHT);
    }

    /// Summe aller Template-Gewichte.
    pub fn total_weight(&self) -> f64 {
        self.counts
            .iter()
            .map(|(_, weight)| f64::from(*weight))
            .sum()
    }

    /// Gewicht eines einzelnen Templates (0.0, falls unbekannt).
    pub fn weight_of(&self, template: TemplateId) -> f64 {
        self.counts
            .iter()
            .find(|(t, _)| *t == template)
            .map_or(0.0, |(_, weight)| f64::from(*weight))
    }

    /// Anzahl unterschiedlicher Templates im Profil.
    pub fn distinct_templates(&self) -> usize {
        self.counts.len()
    }

    /// Surprisal eines Templates gegen dieses Profil, in Bit.
    ///
    /// Nutzt dieselbe Lidstone/Jeffreys-Glättung wie
    /// [`crate::analysis::stats::surprisal`] (dorthin delegiert, damit beide
    /// Pfade exakt dieselbe Formel verwenden), gespeist aus dem gewichteten
    /// statt dem rohen Zählwert. `None`, wenn das Profil noch nicht
    /// vertrauenswürdig genug ist -- der Aufrufer fällt dann auf das
    /// Momentanfenster aus Phase 3 zurück.
    pub fn surprisal(&self, template: TemplateId, alpha: f64, min_weight: f64) -> Option<f64> {
        if !self.is_trusted(min_weight) {
            return None;
        }
        // `surprisal` rundet Gewichte auf ganze Zahlen, da die Funktion für
        // Zählwerte ausgelegt ist; bei typischen Gewichten (>> 1 nach
        // wenigen Beobachtungen) ist der Rundungsfehler vernachlässigbar,
        // und die geteilte Formel wiegt die Konsistenz mit Phase 3 schwerer
        // als eine auf Fließkomma verallgemeinerte Variante.
        let count = self.weight_of(template).round() as u64;
        let total = self.total_weight().round() as u64;
        Some(crate::analysis::stats::surprisal(
            count,
            total,
            self.distinct_templates(),
            alpha,
        ))
    }

    /// Ob das Beobachtungsgewicht die Vertrauensschwelle erreicht.
    pub fn is_trusted(&self, min_weight: f64) -> bool {
        self.total_weight() >= min_weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(n: u64) -> TemplateId {
        TemplateId(n)
    }

    fn config(half_life_buckets: f64, max_templates: usize) -> ProfileConfig {
        ProfileConfig {
            decay: DecayParams::from_buckets(half_life_buckets),
            max_templates,
        }
    }

    #[test]
    fn leeres_profil_ist_nicht_vertrauenswuerdig() {
        let profile = UnitProfile::default();
        assert!(!profile.is_trusted(0.0001));
        assert_eq!(profile.total_weight(), 0.0);
        assert_eq!(profile.distinct_templates(), 0);
    }

    #[test]
    fn gewicht_steigt_mit_wiederholten_beobachtungen() {
        let mut profile = UnitProfile::default();
        let cfg = config(1000.0, 100);
        for _ in 0..5 {
            profile.observe(tid(1), 0, &cfg);
        }
        assert!((profile.weight_of(tid(1)) - 5.0).abs() < 1e-6);
        assert_eq!(profile.distinct_templates(), 1);
    }

    #[test]
    fn zerfall_wirkt_wie_beim_histogramm() {
        let mut profile = UnitProfile::default();
        let cfg = config(10.0, 100);
        profile.observe(tid(1), 0, &cfg);
        profile.observe(tid(1), 10, &cfg);
        assert!(
            (profile.weight_of(tid(1)) - 1.5).abs() < 1e-6,
            "war {}",
            profile.weight_of(tid(1))
        );
    }

    #[test]
    fn obergrenze_verwirft_das_leichteste_template() {
        let mut profile = UnitProfile::default();
        let cfg = config(1000.0, 3);
        // tid(1) wird mehrfach gestärkt, bleibt also das schwerste.
        profile.observe(tid(1), 0, &cfg);
        profile.observe(tid(1), 0, &cfg);
        profile.observe(tid(1), 0, &cfg);
        profile.observe(tid(2), 0, &cfg);
        profile.observe(tid(3), 0, &cfg);
        // Jetzt kommt ein viertes Template hinzu: die Obergrenze (3) ist
        // erreicht, das leichteste (tid(2) oder tid(3), Gewicht 1) muss
        // weichen, tid(1) mit Gewicht 3 bleibt sicher erhalten.
        profile.observe(tid(4), 0, &cfg);

        assert!(profile.distinct_templates() <= 3, "Obergrenze verletzt");
        assert!(
            profile.weight_of(tid(1)) > 0.0,
            "das schwerste Template darf nicht verworfen werden"
        );
    }

    #[test]
    fn surprisal_ohne_ausreichendes_gewicht_ist_none() {
        let mut profile = UnitProfile::default();
        let cfg = config(1000.0, 100);
        profile.observe(tid(1), 0, &cfg);
        assert_eq!(profile.surprisal(tid(1), 0.5, 200.0), None);
    }

    #[test]
    fn surprisal_haeufigen_templates_ist_niedrig() {
        let mut profile = UnitProfile::default();
        let cfg = config(1_000_000.0, 1000);
        for _ in 0..9000 {
            profile.observe(tid(1), 0, &cfg);
        }
        for i in 0..50 {
            profile.observe(tid(2 + i), 0, &cfg);
        }
        let s = profile
            .surprisal(tid(1), 0.5, 200.0)
            .expect("Profil sollte vertrauenswürdig sein");
        assert!(
            s < 1.0,
            "häufiges Template sollte niedriges Surprisal haben, war {s}"
        );
    }

    #[test]
    fn surprisal_stimmt_mit_der_geteilten_formel_ueberein() {
        let mut profile = UnitProfile::default();
        let cfg = config(1_000_000.0, 1000);
        for _ in 0..100 {
            profile.observe(tid(1), 0, &cfg);
        }
        for _ in 0..900 {
            profile.observe(tid(2), 0, &cfg);
        }
        let aus_profil = profile
            .surprisal(tid(1), 0.5, 10.0)
            .expect("vertrauenswürdig");
        let erwartet = crate::analysis::stats::surprisal(100, 1000, 2, 0.5);
        assert!(
            (aus_profil - erwartet).abs() < 1e-6,
            "Profil-Surprisal ({aus_profil}) muss der geteilten Formel ({erwartet}) entsprechen"
        );
    }

    #[test]
    fn neues_unbeobachtetes_template_hat_hohes_surprisal() {
        let mut profile = UnitProfile::default();
        let cfg = config(1_000_000.0, 1000);
        for _ in 0..500 {
            profile.observe(tid(1), 0, &cfg);
        }
        let s = profile
            .surprisal(tid(999), 0.5, 200.0)
            .expect("Profil sollte vertrauenswürdig sein");
        assert!(
            s > 5.0,
            "unbekanntes Template sollte hohes Surprisal haben, war {s}"
        );
    }
}
