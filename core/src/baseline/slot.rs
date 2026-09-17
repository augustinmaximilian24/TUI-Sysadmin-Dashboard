//! Zeitprofil-Slots für Baselines: 48 reguläre Slots (Stunde ×
//! Werktag/Wochenende) plus ein 49. Sammel-Slot [`Slot::ANY`].
//!
//! Siehe `docs/phase4-baselines.md` Abschnitt 4 für die Begründung, warum
//! 48 Slots statt der feineren 24×7-Aufteilung gewählt wurden: bei
//! spärlichen Heimserver-Daten würde die 168-fache Verdünnung dazu führen,
//! dass fast kein Slot je die Vertrauensschwelle erreicht.
//!
//! Die Umrechnung von Zeitstempel auf Slot nutzt die **lokale** Zeitzone,
//! weil das Profil menschliches Verhalten (Bürozeiten, nächtliche Ruhe)
//! abbildet, nicht UTC.

use chrono::{DateTime, Datelike, Local, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

/// Ein Zeitprofil-Slot. Werte 0..=47 sind reguläre Slots
/// (`stunde * 2 + ist_wochenende`), 48 ist [`Slot::ANY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Slot(pub u8);

impl Slot {
    /// Anzahl regulärer Slots (0..=47).
    pub const COUNT: u8 = 48;

    /// Sammel-Slot, der jede Beobachtung zusätzlich zu ihrem regulären Slot
    /// aufnimmt (Fallback-Stufe 2 in der Analyse, siehe
    /// `docs/phase4-baselines.md` Abschnitt 5).
    pub const ANY: Slot = Slot(48);

    /// Bestimmt den Slot aus einem Ereignis-Zeitstempel (Mikrosekunden seit
    /// Unix-Epoch) in der lokalen Zeitzone des Systems.
    ///
    /// Lässt sich der Zeitstempel nicht in eine gültige lokale Zeit
    /// umrechnen (z. B. außerhalb des von `chrono` unterstützten Bereichs),
    /// wird [`Slot::ANY`] geliefert statt zu paniken (Regel 16).
    pub fn from_timestamp_us(timestamp_us: u64) -> Slot {
        match local_datetime(timestamp_us) {
            Some(dt) => {
                let weekend = matches!(dt.weekday(), Weekday::Sat | Weekday::Sun);
                slot_from_hour_and_weekend(dt.hour(), weekend)
            }
            None => Slot::ANY,
        }
    }

    /// Ob dieser Slot ein regulärer Stunden-Slot ist (nicht [`Slot::ANY`]).
    pub fn is_regular(&self) -> bool {
        self.0 < Self::COUNT
    }

    /// Die Stunde (0..23) dieses Slots, sofern regulär.
    pub fn hour(&self) -> Option<u8> {
        self.is_regular().then_some(self.0 / 2)
    }

    /// Ob dieser Slot ein Wochenend-Slot ist, sofern regulär.
    pub fn is_weekend(&self) -> Option<bool> {
        self.is_regular().then_some(self.0 % 2 == 1)
    }
}

/// Reine Abbildung Stunde/Wochenende → Slot, unabhängig von `chrono`s
/// Zeitzonen-Logik und damit ohne Abhängigkeit von der Systemumgebung
/// testbar.
fn slot_from_hour_and_weekend(hour: u32, is_weekend: bool) -> Slot {
    let hour = (hour % 24) as u8;
    Slot(hour * 2 + u8::from(is_weekend))
}

/// Rechnet einen Mikrosekunden-Zeitstempel in eine lokale `DateTime` um.
/// `None` bei Zeitstempeln außerhalb des von `chrono`/`i64` darstellbaren
/// Bereichs (z. B. `u64::MAX`).
fn local_datetime(timestamp_us: u64) -> Option<DateTime<Local>> {
    let secs = i64::try_from(timestamp_us / 1_000_000).ok()?;
    let sub_us = u32::try_from(timestamp_us % 1_000_000).ok()?;
    let utc = DateTime::<Utc>::from_timestamp(secs, sub_us * 1_000)?;
    Some(utc.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_aus_stunde_und_wochenende_ist_im_erwarteten_bereich() {
        // Montag 0 Uhr -> Slot 0, Montag 23 Uhr -> Slot 46 (kein Wochenende).
        assert_eq!(slot_from_hour_and_weekend(0, false), Slot(0));
        assert_eq!(slot_from_hour_and_weekend(23, false), Slot(46));
        // Samstag 0 Uhr -> Slot 1, Samstag 23 Uhr -> Slot 47 (Wochenende).
        assert_eq!(slot_from_hour_and_weekend(0, true), Slot(1));
        assert_eq!(slot_from_hour_and_weekend(23, true), Slot(47));
    }

    #[test]
    fn alle_regulaeren_slots_liegen_unter_der_any_grenze() {
        for hour in 0..24u32 {
            for weekend in [false, true] {
                let slot = slot_from_hour_and_weekend(hour, weekend);
                assert!(slot.0 < Slot::COUNT, "Slot {slot:?} außerhalb 0..48");
                assert!(slot.is_regular());
                assert_ne!(slot, Slot::ANY);
            }
        }
    }

    #[test]
    fn hour_und_is_weekend_sind_umkehrung_der_konstruktion() {
        for hour in 0..24u8 {
            for weekend in [false, true] {
                let slot = slot_from_hour_and_weekend(hour as u32, weekend);
                assert_eq!(slot.hour(), Some(hour));
                assert_eq!(slot.is_weekend(), Some(weekend));
            }
        }
    }

    #[test]
    fn any_slot_hat_keine_stunde_und_kein_wochenende() {
        assert_eq!(Slot::ANY.hour(), None);
        assert_eq!(Slot::ANY.is_weekend(), None);
        assert!(!Slot::ANY.is_regular());
    }

    #[test]
    fn werktag_und_wochenende_ergeben_unterschiedliche_slots() {
        let werktag = slot_from_hour_and_weekend(14, false);
        let wochenende = slot_from_hour_and_weekend(14, true);
        assert_ne!(werktag, wochenende);
    }

    #[test]
    fn stunden_wechsel_ergibt_unterschiedliche_slots() {
        let neun = slot_from_hour_and_weekend(9, false);
        let zehn = slot_from_hour_and_weekend(10, false);
        assert_ne!(neun, zehn);
    }

    #[test]
    fn timestamp_ausserhalb_des_darstellbaren_bereichs_faellt_auf_any_zurueck() {
        // u64::MAX Mikrosekunden sprengt jeden i64-Sekunden-Zeitstempel.
        assert_eq!(Slot::from_timestamp_us(u64::MAX), Slot::ANY);
    }

    #[test]
    fn gewoehnlicher_zeitstempel_ergibt_einen_regulaeren_slot() {
        // 2026-01-15 12:00:00 UTC, ein beliebiger Zeitpunkt im gültigen
        // Bereich; unabhängig von der Test-Zeitzone muss ein regulärer
        // Slot herauskommen, kein Fallback auf ANY.
        let timestamp_us = 1_768_478_400_000_000u64;
        let slot = Slot::from_timestamp_us(timestamp_us);
        assert!(slot.is_regular(), "Slot {slot:?} sollte regulär sein");
    }

    #[test]
    fn gleicher_zeitstempel_ergibt_immer_denselben_slot() {
        let timestamp_us = 1_768_478_400_000_000u64;
        let a = Slot::from_timestamp_us(timestamp_us);
        let b = Slot::from_timestamp_us(timestamp_us);
        assert_eq!(a, b);
    }

    /// Regressionstest für die Sommerzeitumstellung: Der Slot wird pro
    /// Ereignis aus dessen eigenem Zeitstempel bestimmt (nicht einmalig
    /// beim Programmstart gecacht), sodass sich die Stunden-Zuordnung an
    /// der Umstellung korrekt verschiebt. Feste Zeitzone über die
    /// `TZ`-Umgebungsvariable, damit der Test unabhängig von der
    /// Systemzeitzone deterministisch ist.
    #[test]
    fn sommerzeitumstellung_wird_pro_ereignis_neu_berechnet() {
        // SAFETY: Nur dieser Test in der gesamten Codebasis setzt TZ; keine
        // andere Testfunktion liest oder schreibt diese Variable.
        unsafe {
            std::env::set_var("TZ", "Europe/Berlin");
        }

        // 2026-03-29 ist der Umstellungstag Winter->Sommer in Europa: um
        // 01:00 UTC springt die lokale Uhr von 01:59 CET auf 03:00 CEST.
        // Vor der Umstellung: 2026-03-29T00:30:00Z = 01:30 CET -> Stunde 1.
        let vor_umstellung_us = 1_774_742_400_000_000u64 + 30 * 60 * 1_000_000;
        // Nach der Umstellung: 2026-03-29T01:30:00Z = 03:30 CEST -> Stunde 3.
        let nach_umstellung_us = 1_774_742_400_000_000u64 + 90 * 60 * 1_000_000;

        let vorher = Slot::from_timestamp_us(vor_umstellung_us);
        let nachher = Slot::from_timestamp_us(nach_umstellung_us);

        unsafe {
            std::env::remove_var("TZ");
        }

        assert_eq!(
            vorher.hour(),
            Some(1),
            "vor der Umstellung 01:30 CET erwartet"
        );
        assert_eq!(
            nachher.hour(),
            Some(3),
            "nach der Umstellung 03:30 CEST erwartet"
        );
    }
}
