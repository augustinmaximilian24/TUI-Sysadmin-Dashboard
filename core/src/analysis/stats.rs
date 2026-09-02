//! Reine statistische Funktionen für die Anomalie-Erkennung.
//!
//! Alle Funktionen hier sind frei von Zustand und Seiteneffekten und damit
//! direkt mit festen Fixtures testbar (Regel 24). Bewusst wird durchgehend
//! mit *robusten* Schätzern (Median, MAD) gearbeitet: Log-Raten sind stark
//! rechtsschief und enthalten regelmäßig Ausreißer; Mittelwert und
//! Standardabweichung würden von genau den Ereignissen verzerrt, die wir
//! erkennen wollen (ein einzelner Burst hebt σ so weit an, dass der Burst
//! selbst unauffällig wird).

/// Skalierungskonstante, die die MAD einer normalverteilten Stichprobe auf
/// deren Standardabweichung abbildet: für X ~ N(µ, σ²) gilt
/// MAD ≈ 0.6745·σ. Der robuste Z-Score wird damit auf derselben Skala
/// interpretierbar wie ein klassischer Z-Score.
const MAD_TO_SIGMA: f64 = 0.6745;

/// Obergrenze für den zurückgegebenen Z-Score. Ohne Deckelung liefern
/// Verteilungen mit MAD = 0 einen unendlichen Score, der sich weder
/// anzeigen noch gewichten lässt.
const Z_SCORE_SATURATION: f64 = 50.0;

/// Sinnvolle Skalen-Untergrenze für Zählwerte (Ereignisse pro Bucket).
/// Zählwerte sind ganzzahlig; eine Streuung unterhalb von einem Ereignis
/// ist nicht interpretierbar.
pub const COUNT_MIN_SCALE: f64 = 1.0;

/// Sinnvolle Skalen-Untergrenze für Entropiewerte in Bit. Die Entropie
/// eines realen Log-Stroms schwankt auch im Normalbetrieb um etwa diese
/// Größenordnung; ein kleinerer Wert würde jede Mikroschwankung zum Alarm
/// erheben.
pub const ENTROPY_MIN_SCALE: f64 = 0.15;

/// Poisson-bewusste Skalen-Untergrenze für eine Zähl-Historie.
///
/// Log-Ereignisse eines Templates treten näherungsweise als Poisson-Prozess
/// auf; dessen Standardabweichung ist √λ, nicht 1. Eine feste Untergrenze
/// von 1.0 würde bei einer typischen Rate von 2 Ereignissen pro Bucket schon
/// den gewöhnlichen Ausschlag auf 5 als 3σ-Ereignis werten – im Dauerbetrieb
/// eine verlässliche Fehlalarmquelle. Die Untergrenze ist daher
/// `max(1, √(median + 1))`; das `+1` hält den Wert auch bei einem Median von
/// 0 sinnvoll definiert.
pub fn count_scale_floor(values: &[f64]) -> f64 {
    let med = median(values).unwrap_or(0.0).max(0.0);
    (med + 1.0).sqrt().max(COUNT_MIN_SCALE)
}

/// Berechnet die Shannon-Entropie H = -Σ pᵢ·log₂(pᵢ) über eine Häufigkeits-
/// verteilung, in Bit.
///
/// Ein Wert nahe 0 bedeutet, dass fast alle Ereignisse auf dasselbe Template
/// entfallen (typisch für einen Log-Sturm einer einzelnen Quelle); hohe Werte
/// bedeuten eine breit gestreute, „normale" Mischung. Leere Eingaben und
/// Nullzählungen ergeben 0.
pub fn shannon_entropy(counts: &[u64]) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let total_f = total as f64;
    let sum: f64 = counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / total_f;
            p * p.log2()
        })
        .sum();
    // Die Addition von 0.0 normalisiert das mögliche -0.0 (bei genau einer
    // Kategorie) zu +0.0.
    -sum + 0.0
}

/// Maximal mögliche Entropie bei `distinct` gleichverteilten Kategorien,
/// also log₂(distinct). Nützlich, um die Entropie auf 0..1 zu normieren.
pub fn max_entropy(distinct: usize) -> f64 {
    if distinct <= 1 {
        0.0
    } else {
        (distinct as f64).log2()
    }
}

/// Median einer Stichprobe. Liefert `None` für eine leere Stichprobe.
///
/// Sortiert intern eine Kopie; für die hier auftretenden Historienlängen
/// (Größenordnung 10²) ist das unkritisch.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    // `total_cmp` ist eine totale Ordnung auch für NaN und vermeidet damit
    // das Panic-Risiko von `partial_cmp().unwrap()` (Regel 15).
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let mid = n / 2;
    if n.is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

/// Median der absoluten Abweichungen vom Median (MAD).
/// Liefert `None` für eine leere Stichprobe.
pub fn median_absolute_deviation(values: &[f64]) -> Option<f64> {
    let med = median(values)?;
    let deviations: Vec<f64> = values.iter().map(|v| (v - med).abs()).collect();
    median(&deviations)
}

/// Robuster Z-Score von `value` bezogen auf die Stichprobe `values`.
///
/// Verwendet Median und MAD statt Mittelwert und σ. `min_scale` ist die
/// kleinste Streuung, die noch als bedeutsam gilt (siehe
/// [`COUNT_MIN_SCALE`] und [`ENTROPY_MIN_SCALE`]).
///
/// Der Skalenschätzer ist `max(MAD / 0.6745, min_scale)`. Die Untergrenze
/// löst den in der Praxis häufigsten Sonderfall: Hat mehr als die Hälfte
/// der Historie exakt denselben Wert – bei seltenen Templates typischerweise
/// lauter Nullen –, ist die MAD exakt 0 und der Z-Score ohne Untergrenze
/// unendlich.
///
/// Bewusst *nicht* verwendet wird die verbreitete Rückfallebene über die
/// mittlere absolute Abweichung (MeanAD): Sie ist selbst nicht robust. Eine
/// Historie wie `[1, 1, 1, 1, 1, 1, 1, 1000]` bekäme dadurch eine Skala von
/// über 100, sodass ein Anstieg von 1 auf 20 als unauffällig gälte – genau
/// die Verzerrung durch Ausreißer, wegen der hier überhaupt robuste
/// Schätzer eingesetzt werden.
///
/// Sonderfälle: leere Stichprobe → 0.0 (keine Aussage möglich, kein Alarm).
/// Das Ergebnis wird stets auf ±[`Z_SCORE_SATURATION`] begrenzt.
pub fn robust_z_score(value: f64, values: &[f64], min_scale: f64) -> f64 {
    let Some(med) = median(values) else {
        return 0.0;
    };
    let deviation = value - med;

    let deviations: Vec<f64> = values.iter().map(|v| (v - med).abs()).collect();
    let mad = median(&deviations).unwrap_or(0.0);

    let scale = (mad / MAD_TO_SIGMA).max(min_scale.max(f64::MIN_POSITIVE));
    (deviation / scale).clamp(-Z_SCORE_SATURATION, Z_SCORE_SATURATION)
}

/// Surprisal (Informationsgehalt) eines Templates in Bit: -log₂ P(template).
///
/// `count` ist die beobachtete Häufigkeit des Templates, `total` die Summe
/// aller Beobachtungen, `distinct` die Anzahl unterschiedlicher bekannter
/// Templates. Die Wahrscheinlichkeit wird additiv geglättet
/// (Lidstone/Jeffreys):
///
/// ```text
/// P = (count + α) / (total + α·(distinct + 1))
/// ```
///
/// Das zusätzliche `+1` im Nenner reserviert Wahrscheinlichkeitsmasse für
/// „noch nie gesehen". Ohne Glättung wäre P = 0 für ein neues Template und
/// das Surprisal unendlich – genau der Fall, der bei jedem Neustart und bei
/// jedem harmlosen neuen Dienst auftritt. Ein erstmals gesehenes Template
/// (`count` = 0) bekommt so einen hohen, aber endlichen Wert, der mit der
/// Menge der bisherigen Beobachtungen wächst.
pub fn surprisal(count: u64, total: u64, distinct: usize, alpha: f64) -> f64 {
    let alpha = alpha.max(f64::EPSILON);
    let numerator = count as f64 + alpha;
    let denominator = total as f64 + alpha * (distinct as f64 + 1.0);
    if denominator <= 0.0 {
        return 0.0;
    }
    let p = (numerator / denominator).clamp(f64::MIN_POSITIVE, 1.0);
    -p.log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Toleranz für Fließkomma-Vergleiche in den Tests.
    const EPS: f64 = 1e-9;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn entropie_einer_einzelnen_kategorie_ist_null() {
        assert!(shannon_entropy(&[42]).abs() < EPS);
    }

    #[test]
    fn entropie_zweier_gleichverteilter_kategorien_ist_ein_bit() {
        assert!(approx(shannon_entropy(&[10, 10]), 1.0));
    }

    #[test]
    fn entropie_vierer_gleichverteilter_kategorien_ist_zwei_bit() {
        assert!(approx(shannon_entropy(&[5, 5, 5, 5]), 2.0));
    }

    #[test]
    fn entropie_ist_bei_schiefer_verteilung_kleiner_als_bei_gleichverteilung() {
        let gleich = shannon_entropy(&[25, 25, 25, 25]);
        let schief = shannon_entropy(&[97, 1, 1, 1]);
        assert!(schief < gleich);
        assert!(schief > 0.0);
    }

    #[test]
    fn entropie_leerer_verteilung_ist_null() {
        assert!(shannon_entropy(&[]).abs() < EPS);
        assert!(shannon_entropy(&[0, 0]).abs() < EPS);
    }

    #[test]
    fn max_entropie_entspricht_log2_der_kategorienzahl() {
        assert!(approx(max_entropy(8), 3.0));
        assert!(approx(max_entropy(1), 0.0));
        assert!(approx(max_entropy(0), 0.0));
    }

    #[test]
    fn median_bei_ungerader_anzahl() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
    }

    #[test]
    fn median_bei_gerader_anzahl_mittelt_die_beiden_mittleren() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn median_leerer_stichprobe_ist_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn mad_einer_konstanten_stichprobe_ist_null() {
        let mad = median_absolute_deviation(&[5.0, 5.0, 5.0, 5.0]).expect("nicht leer");
        assert!(mad.abs() < EPS);
    }

    #[test]
    fn mad_berechnet_median_der_absoluten_abweichungen() {
        // Median von [1,2,3,4,5] = 3; Abweichungen [2,1,0,1,2]; deren Median = 1.
        let mad = median_absolute_deviation(&[1.0, 2.0, 3.0, 4.0, 5.0]).expect("nicht leer");
        assert!(approx(mad, 1.0));
    }

    #[test]
    fn z_score_am_median_ist_null() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!(robust_z_score(3.0, &values, COUNT_MIN_SCALE).abs() < EPS);
    }

    #[test]
    fn z_score_erkennt_ausreisser_nach_oben() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let z = robust_z_score(100.0, &values, COUNT_MIN_SCALE);
        assert!(z > 10.0, "Ausreißer sollte deutlich positiven Z-Score haben, war {z}");
        assert!(z <= 50.0, "Z-Score muss gedeckelt sein, war {z}");
    }

    #[test]
    fn z_score_ist_robust_gegen_einzelnen_ausreisser_in_der_historie() {
        // Ein einzelner extremer Wert in der Historie darf die Skala nicht
        // so aufblähen, dass ein echter Burst unauffällig wird -- genau der
        // Schwachpunkt von Mittelwert/σ und auch der MeanAD-Rückfallebene.
        let mit_ausreisser = [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1000.0];
        let z = robust_z_score(20.0, &mit_ausreisser, COUNT_MIN_SCALE);
        assert!(z > 3.0, "Burst sollte trotz Ausreißer in der Historie auffallen, war {z}");
    }

    #[test]
    fn z_score_bei_mad_null_nutzt_die_skalen_untergrenze() {
        // Median 0, MAD 0 (Mehrheit ist 0): die Untergrenze verhindert
        // Division durch null, ohne die Empfindlichkeit zu verlieren.
        let values = [0.0, 0.0, 0.0, 0.0, 0.0, 3.0, 4.0];
        let z = robust_z_score(10.0, &values, COUNT_MIN_SCALE);
        assert!(z.is_finite());
        assert!(approx(z, 10.0), "erwartet 10/1.0, war {z}");
    }

    #[test]
    fn z_score_konstanter_historie_bleibt_endlich_und_verhaeltnismaessig() {
        let values = [7.0, 7.0, 7.0, 7.0];
        assert!(robust_z_score(7.0, &values, COUNT_MIN_SCALE).abs() < EPS);
        // Kleine Abweichung -> kleiner Score. Eine Sättigung auf 50 wäre hier
        // ein Fehlalarm-Generator: von 7 auf 9 ist keine Anomalie.
        let z = robust_z_score(9.0, &values, COUNT_MIN_SCALE);
        assert!(z.is_finite(), "darf nicht unendlich werden");
        assert!(approx(z, 2.0), "erwartet 2/1.0, war {z}");
    }

    #[test]
    fn z_score_saettigt_bei_extremer_abweichung_von_konstanter_historie() {
        let values = [0.0, 0.0, 0.0, 0.0];
        let z = robust_z_score(5000.0, &values, COUNT_MIN_SCALE);
        assert!(approx(z, 50.0), "muss auf 50 gedeckelt sein, war {z}");
    }

    #[test]
    fn groessere_min_scale_daempft_den_score() {
        let values = [2.0, 2.0, 2.0, 2.0];
        let fein = robust_z_score(2.6, &values, 0.15);
        let grob = robust_z_score(2.6, &values, 1.0);
        assert!(fein > grob, "kleinere Untergrenze muss empfindlicher sein");
    }

    #[test]
    fn z_score_leerer_historie_ist_null() {
        assert!(robust_z_score(5.0, &[], COUNT_MIN_SCALE).abs() < EPS);
    }

    #[test]
    fn surprisal_ist_fuer_neues_template_hoch_aber_endlich() {
        let s = surprisal(0, 10_000, 50, 0.5);
        assert!(s.is_finite());
        assert!(s > 10.0, "unbekanntes Template sollte hohes Surprisal haben, war {s}");
    }

    #[test]
    fn surprisal_ist_fuer_haeufiges_template_niedrig() {
        let s = surprisal(9_000, 10_000, 50, 0.5);
        assert!(s < 1.0, "dominantes Template sollte niedriges Surprisal haben, war {s}");
    }

    #[test]
    fn surprisal_faellt_monoton_mit_steigender_haeufigkeit() {
        let selten = surprisal(1, 10_000, 50, 0.5);
        let mittel = surprisal(100, 10_000, 50, 0.5);
        let haeufig = surprisal(5_000, 10_000, 50, 0.5);
        assert!(selten > mittel);
        assert!(mittel > haeufig);
    }

    #[test]
    fn surprisal_ohne_beobachtungen_ist_endlich() {
        let s = surprisal(0, 0, 0, 0.5);
        assert!(s.is_finite(), "war {s}");
    }
}
