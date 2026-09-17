//! Export der geladenen Anomalien als JSON/CSV „zur Weiterverarbeitung"
//! (Phase 10, optional). Reine Formatierung, getrennt vom Schreiben, damit
//! sie ohne Dateisystemzugriff testbar ist.

use logsentry_proto::AnomalyEvent;

/// Baut den JSON-Export: ein Array vollständiger `AnomalyEvent`-Objekte.
pub fn anomalies_to_json(anomalies: &[AnomalyEvent]) -> serde_json::Result<String> {
    serde_json::to_string_pretty(anomalies)
}

/// Baut den CSV-Export mit einer festen Spaltenauswahl (Rohnachricht und
/// Template-Text sind die einzigen Felder, die Kommas/Anführungszeichen
/// enthalten können und werden entsprechend RFC 4180 maskiert).
pub fn anomalies_to_csv(anomalies: &[AnomalyEvent]) -> String {
    let mut out = String::from(
        "id,timestamp_us,level,unit,pid,template_id,template_text,sample_message,score\n",
    );
    for a in anomalies {
        out.push_str(&format!(
            "{},{},{:?},{},{},{},{},{},{:.4}\n",
            a.id,
            a.timestamp_us,
            a.level,
            csv_field(a.unit.as_deref().unwrap_or("")),
            a.pid.map_or(String::new(), |p| p.to_string()),
            a.template_id,
            csv_field(&a.template_text),
            csv_field(&a.sample_message),
            a.breakdown.combined,
        ));
    }
    out
}

/// Maskiert ein CSV-Feld nach RFC 4180, sofern nötig: in Anführungszeichen
/// einschließen, sobald es Komma, Anführungszeichen oder Zeilenumbruch
/// enthält; enthaltene Anführungszeichen werden verdoppelt.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logsentry_proto::{AnomalyLevel, RateSource, ScoreBreakdown};

    fn sample(id: u64, unit: Option<&str>, message: &str) -> AnomalyEvent {
        AnomalyEvent {
            id,
            timestamp_us: 1000,
            template_id: 7,
            template_text: "Failed password for <USER>".to_string(),
            sample_message: message.to_string(),
            unit: unit.map(str::to_string),
            pid: Some(123),
            priority: Some(4),
            level: AnomalyLevel::Warn,
            breakdown: ScoreBreakdown {
                rate_z: 1.0,
                surprisal_bits: 2.0,
                entropy_z: 0.5,
                rate_component: 0.3,
                surprisal_component: 0.3,
                entropy_component: 0.1,
                combined: 0.6789,
                rate_source: RateSource::ShortTerm,
            },
            suppressed_since_last: 0,
        }
    }

    #[test]
    fn json_export_ist_ein_gueltiges_array_mit_allen_feldern() {
        let anomalies = vec![sample(1, Some("sshd.service"), "einfache Zeile")];
        let json = anomalies_to_json(&anomalies).unwrap();
        let parsed: Vec<AnomalyEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, anomalies);
    }

    #[test]
    fn csv_export_maskiert_kommas_und_anfuehrungszeichen() {
        let anomalies = vec![sample(
            1,
            Some("sshd.service"),
            "Zeile mit, Komma und \"Zitat\"",
        )];
        let csv = anomalies_to_csv(&anomalies);
        assert!(csv.contains("\"Zeile mit, Komma und \"\"Zitat\"\"\""));
    }

    #[test]
    fn csv_export_hat_kopfzeile_und_eine_zeile_je_anomalie() {
        let anomalies = vec![
            sample(1, Some("sshd.service"), "eins"),
            sample(2, None, "zwei"),
        ];
        let csv = anomalies_to_csv(&anomalies);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("id,timestamp_us"));
        assert!(lines[2].contains(",,")); // fehlende Unit bei id=2
    }
}
