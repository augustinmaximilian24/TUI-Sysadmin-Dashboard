//! Statische Anomalie-Masken für wiederkehrende Wartungsmeldungen.
//!
//! Ergänzt den Selbstfilter (Regel 13) und das laufzeit-getriggerte
//! `MuteAnomaly` (Phase 8) um eine dritte, konfigurationsbasierte Stufe:
//! Ereignisse, die auf eine [`QuietTemplateRule`](crate::config::QuietTemplateRule)
//! passen, erreichen die Analyse-Engine gar nicht erst. Anders als ein
//! Mute-per-Template-ID ist das robust gegen Änderungen an der Token-
//! Maskierung (die Template-ID würde sich sonst verschieben) und wirkt
//! schon beim allerersten Auftreten einer Meldung, nicht erst nachdem sie
//! einmal als Anomalie sichtbar wurde und manuell stummgeschaltet wurde.

use regex::Regex;

use crate::config::QuietConfig;

/// Kompilierte Fassung der [`QuietConfig`]: eine Liste aus optionalem
/// Unit-Filter und Regex, in Prüfreihenfolge.
///
/// Ungültige Regex-Muster führen nicht zum Absturz des Daemons (Regel 16):
/// sie werden beim Erstellen verworfen und über `tracing::warn!`
/// protokolliert, der Rest der Konfiguration bleibt wirksam.
pub struct QuietFilter {
    rules: Vec<(Option<String>, Regex)>,
}

impl QuietFilter {
    /// Kompiliert alle Regeln aus der Konfiguration.
    pub fn new(config: &QuietConfig) -> Self {
        let rules = config
            .templates
            .iter()
            .filter_map(|rule| match Regex::new(&rule.pattern) {
                Ok(re) => Some((rule.unit.clone(), re)),
                Err(err) => {
                    tracing::warn!(
                        muster = %rule.pattern,
                        fehler = %err,
                        "ungueltiges Muster in [[quiet.templates]] wird ignoriert"
                    );
                    None
                }
            })
            .collect();
        Self { rules }
    }

    /// Ob dieses Ereignis von einer Anomalie-Maske erfasst wird.
    pub fn is_quiet(&self, unit: Option<&str>, message: &str) -> bool {
        self.rules.iter().any(|(rule_unit, pattern)| {
            let unit_matches = match rule_unit {
                Some(expected) => unit == Some(expected.as_str()),
                None => true,
            };
            unit_matches && pattern.is_match(message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QuietTemplateRule;

    fn filter_with(rules: Vec<QuietTemplateRule>) -> QuietFilter {
        QuietFilter::new(&QuietConfig { templates: rules })
    }

    #[test]
    fn leere_konfiguration_maskiert_nichts() {
        let filter = filter_with(Vec::new());
        assert!(!filter.is_quiet(Some("init.scope"), "irgendeine Zeile"));
    }

    #[test]
    fn regel_ohne_unit_greift_unabhaengig_von_der_unit() {
        let filter = filter_with(vec![QuietTemplateRule {
            unit: None,
            pattern: r"^Testmuster$".to_string(),
        }]);
        assert!(filter.is_quiet(Some("beliebig.service"), "Testmuster"));
        assert!(filter.is_quiet(None, "Testmuster"));
    }

    #[test]
    fn regel_mit_unit_verlangt_exakte_uebereinstimmung() {
        let filter = filter_with(vec![QuietTemplateRule {
            unit: Some("init.scope".to_string()),
            pattern: r"^Testmuster$".to_string(),
        }]);
        assert!(filter.is_quiet(Some("init.scope"), "Testmuster"));
        assert!(!filter.is_quiet(Some("anderes.service"), "Testmuster"));
        assert!(!filter.is_quiet(None, "Testmuster"));
    }

    #[test]
    fn nicht_passendes_muster_maskiert_nicht() {
        let filter = filter_with(vec![QuietTemplateRule {
            unit: None,
            pattern: r"^Testmuster$".to_string(),
        }]);
        assert!(!filter.is_quiet(None, "voellig andere Zeile"));
    }

    #[test]
    fn ungueltiges_muster_wird_verworfen_statt_zu_paniken() {
        let filter = filter_with(vec![QuietTemplateRule {
            unit: None,
            pattern: r"(unbalanciert".to_string(),
        }]);
        assert!(!filter.is_quiet(None, "(unbalanciert"));
    }

    #[test]
    fn default_regeln_maskieren_die_am_2026_09_21_identifizierten_wartungszeilen() {
        let filter = QuietFilter::new(&QuietConfig::default());
        let faelle = [
            (Some("init.scope"), "run-timeshift-271289-backup.mount: Deactivated successfully."),
            (Some("init.scope"), "Starting dpkg-db-backup.service - Daily dpkg database backup service..."),
            (Some("init.scope"), "dpkg-db-backup.service: Deactivated successfully."),
            (Some("init.scope"), "Starting logrotate.service - Rotate log files..."),
            (Some("init.scope"), "Finished logrotate.service - Rotate log files."),
            (Some("init.scope"), "Starting man-db.service - Daily man-db regeneration..."),
            (Some("init.scope"), "man-db.service: Deactivated successfully."),
            (Some("init.scope"), "flatpak-system-helper.service: Consumed 2.496s CPU time."),
            (Some("init.scope"), "flatpak-system-helper.service: Deactivated successfully."),
            (Some("cron.service"), "(root) CMD (cd / && run-parts --report /etc/cron.hourly)"),
            (Some("cron.service"), "(root) LIST (root)"),
        ];
        for (unit, message) in faelle {
            assert!(
                filter.is_quiet(unit, message),
                "muss maskiert werden: {message:?}"
            );
        }
    }

    #[test]
    fn default_regeln_maskieren_keine_echten_anomalien() {
        let filter = QuietFilter::new(&QuietConfig::default());
        let faelle = [
            (Some("sshd.service"), "Failed password for root from 203.0.113.9 port 51422 ssh2"),
            (Some("cups.service"), "cups-browsed.service: State 'stop-sigterm' timed out. Killing."),
            (None, "kernel: Out of memory: Killed process 5321"),
            (Some("cron.service"), "(root) CMD (curl http://example.invalid/x)"),
        ];
        for (unit, message) in faelle {
            assert!(
                !filter.is_quiet(unit, message),
                "darf nicht maskiert werden: {message:?}"
            );
        }
    }
}
