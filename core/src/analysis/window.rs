//! Gleitendes Zeitfenster über die zuletzt gesehenen Log-Ereignisse.
//!
//! Der Puffer hält Ereignisse der letzten `window_seconds` und liefert die
//! Häufigkeitsverteilung der Templates darin – die Grundlage für Entropie
//! und Surprisal. Er hat zwei unabhängige Obergrenzen (Regel 18): die
//! zeitliche (Fensterlänge) und eine harte Anzahl-Obergrenze, die auch bei
//! einem extremen Log-Sturm innerhalb des Zeitfensters greift.
//!
//! Die Zeit wird ausschließlich aus den Ereignis-Zeitstempeln abgeleitet,
//! nie aus der Wanduhr. Nur so liefert der Replay-Modus (Regel 28) exakt
//! dieselben Ergebnisse wie der Live-Betrieb.

use std::collections::{HashMap, VecDeque};

use crate::template::TemplateId;

/// Ein Eintrag im Zeitfenster.
#[derive(Debug, Clone, Copy)]
struct WindowEntry {
    timestamp_us: u64,
    template_id: TemplateId,
}

/// Gleitendes Zeitfenster mit Häufigkeitszählung pro Template.
pub struct SlidingWindow {
    entries: VecDeque<WindowEntry>,
    counts: HashMap<TemplateId, u64>,
    window_us: u64,
    max_entries: usize,
    /// Höchster bisher gesehener Zeitstempel; dient als „Jetzt" für die
    /// Verdrängung. Ereignisse können leicht außer der Reihe eintreffen,
    /// daher wird bewusst das Maximum und nicht der letzte Wert verwendet.
    newest_us: u64,
    /// Anzahl der Einträge, die wegen der harten Anzahl-Obergrenze (nicht
    /// wegen Zeitablauf) verdrängt wurden. Für die Anzeige in der GUI.
    dropped_by_capacity: u64,
    /// Anzahl der Ereignisse, die bereits beim Eintreffen zu alt für das
    /// Fenster waren.
    dropped_late: u64,
}

impl SlidingWindow {
    /// Erstellt ein neues Zeitfenster.
    ///
    /// `window_seconds` legt die zeitliche Tiefe fest, `max_entries` die
    /// harte Obergrenze an gleichzeitig gehaltenen Ereignissen.
    pub fn new(window_seconds: u64, max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            counts: HashMap::new(),
            window_us: window_seconds.saturating_mul(1_000_000),
            max_entries: max_entries.max(1),
            newest_us: 0,
            dropped_by_capacity: 0,
            dropped_late: 0,
        }
    }

    /// Nimmt ein Ereignis auf und verdrängt alles, was aus dem Fenster
    /// gefallen ist oder die Kapazitätsgrenze überschreitet.
    ///
    /// Ereignisse, die bereits beim Eintreffen älter als das Fenster sind
    /// (stark verspätete Zustellung), werden gar nicht erst aufgenommen und
    /// nur gezählt. Sonst blieben sie am Ende der Deque liegen, wo die
    /// kopfseitige Verdrängung sie nie erreicht.
    pub fn push(&mut self, timestamp_us: u64, template_id: TemplateId) {
        self.newest_us = self.newest_us.max(timestamp_us);

        if timestamp_us < self.newest_us.saturating_sub(self.window_us) {
            self.dropped_late += 1;
            return;
        }

        self.entries.push_back(WindowEntry {
            timestamp_us,
            template_id,
        });
        *self.counts.entry(template_id).or_insert(0) += 1;

        self.evict_expired();
        self.evict_over_capacity();
    }

    /// Entfernt alle Einträge, die älter als das Zeitfenster sind.
    fn evict_expired(&mut self) {
        let cutoff = self.newest_us.saturating_sub(self.window_us);
        while let Some(front) = self.entries.front() {
            if front.timestamp_us >= cutoff {
                break;
            }
            let removed = *front;
            self.entries.pop_front();
            self.decrement_count(removed.template_id);
        }
    }

    /// Erzwingt die harte Anzahl-Obergrenze, unabhängig von der Zeit.
    fn evict_over_capacity(&mut self) {
        while self.entries.len() > self.max_entries {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.decrement_count(removed.template_id);
            self.dropped_by_capacity += 1;
        }
    }

    /// Verringert den Zähler eines Templates und entfernt den Eintrag ganz,
    /// wenn er auf 0 fällt (sonst wüchse die Map unbegrenzt).
    fn decrement_count(&mut self, template_id: TemplateId) {
        if let Some(count) = self.counts.get_mut(&template_id) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&template_id);
            }
        }
    }

    /// Anzahl der Ereignisse im Fenster.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Ob das Fenster leer ist.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Anzahl unterschiedlicher Templates im Fenster.
    pub fn distinct_templates(&self) -> usize {
        self.counts.len()
    }

    /// Häufigkeit eines bestimmten Templates im Fenster.
    pub fn count_of(&self, template_id: TemplateId) -> u64 {
        self.counts.get(&template_id).copied().unwrap_or(0)
    }

    /// Anzahl wegen der Kapazitätsgrenze verdrängter Einträge.
    pub fn dropped_by_capacity(&self) -> u64 {
        self.dropped_by_capacity
    }

    /// Anzahl verworfener Ereignisse, die schon beim Eintreffen zu alt waren.
    pub fn dropped_late(&self) -> u64 {
        self.dropped_late
    }

    /// Shannon-Entropie der Template-Verteilung im Fenster, in Bit.
    pub fn entropy(&self) -> f64 {
        let counts: Vec<u64> = self.counts.values().copied().collect();
        super::stats::shannon_entropy(&counts)
    }

    /// Auf 0..1 normierte Entropie (Entropie geteilt durch die maximal
    /// mögliche Entropie bei gleicher Anzahl Templates). Erleichtert den
    /// Vergleich über Fenster mit unterschiedlich vielen Templates hinweg.
    pub fn normalized_entropy(&self) -> f64 {
        let max = super::stats::max_entropy(self.counts.len());
        if max <= 0.0 {
            0.0
        } else {
            (self.entropy() / max).clamp(0.0, 1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000;

    fn tid(n: u64) -> TemplateId {
        TemplateId(n)
    }

    #[test]
    fn neues_fenster_ist_leer() {
        let window = SlidingWindow::new(60, 1000);
        assert!(window.is_empty());
        assert_eq!(window.len(), 0);
        assert_eq!(window.distinct_templates(), 0);
    }

    #[test]
    fn zaehlt_templates_im_fenster() {
        let mut window = SlidingWindow::new(60, 1000);
        window.push(SEC, tid(1));
        window.push(2 * SEC, tid(1));
        window.push(3 * SEC, tid(2));

        assert_eq!(window.len(), 3);
        assert_eq!(window.count_of(tid(1)), 2);
        assert_eq!(window.count_of(tid(2)), 1);
        assert_eq!(window.distinct_templates(), 2);
    }

    #[test]
    fn verdraengt_eintraege_ausserhalb_des_zeitfensters() {
        let mut window = SlidingWindow::new(10, 1000);
        window.push(SEC, tid(1));
        window.push(2 * SEC, tid(1));
        // Sprung weit nach vorn: die alten Einträge fallen aus dem Fenster.
        window.push(100 * SEC, tid(2));

        assert_eq!(window.len(), 1);
        assert_eq!(window.count_of(tid(1)), 0);
        assert_eq!(window.count_of(tid(2)), 1);
    }

    #[test]
    fn entfernt_template_aus_der_map_wenn_zaehler_auf_null_faellt() {
        let mut window = SlidingWindow::new(10, 1000);
        window.push(SEC, tid(1));
        window.push(100 * SEC, tid(2));
        // tid(1) darf nicht als Leiche mit Zähler 0 zurückbleiben.
        assert_eq!(window.distinct_templates(), 1);
    }

    #[test]
    fn harte_kapazitaetsgrenze_greift_auch_innerhalb_des_zeitfensters() {
        let mut window = SlidingWindow::new(3600, 3);
        for i in 0..10 {
            window.push(i * SEC, tid(1));
        }
        assert_eq!(window.len(), 3, "Kapazitätsgrenze muss eingehalten werden");
        assert_eq!(window.dropped_by_capacity(), 7);
    }

    #[test]
    fn entropie_ist_null_bei_nur_einem_template() {
        let mut window = SlidingWindow::new(60, 1000);
        for i in 0..10 {
            window.push(i * SEC, tid(1));
        }
        assert!(window.entropy().abs() < 1e-9);
    }

    #[test]
    fn entropie_steigt_mit_der_vielfalt() {
        let mut einheitlich = SlidingWindow::new(60, 1000);
        let mut gemischt = SlidingWindow::new(60, 1000);
        for i in 0..8 {
            einheitlich.push(i * SEC, tid(1));
            gemischt.push(i * SEC, tid(i));
        }
        assert!(gemischt.entropy() > einheitlich.entropy());
    }

    #[test]
    fn normierte_entropie_liegt_zwischen_null_und_eins() {
        let mut window = SlidingWindow::new(60, 1000);
        for i in 0..8 {
            window.push(i * SEC, tid(i % 3));
        }
        let normalized = window.normalized_entropy();
        assert!((0.0..=1.0).contains(&normalized), "war {normalized}");
    }

    #[test]
    fn ereignisse_ausser_der_reihe_verschieben_das_fenster_nicht_zurueck() {
        let mut window = SlidingWindow::new(10, 1000);
        window.push(100 * SEC, tid(1));
        // Ein verspätetes, sehr altes Ereignis darf das Fenster nicht
        // zurücksetzen und auch nicht dauerhaft darin liegen bleiben.
        window.push(SEC, tid(2));
        assert_eq!(window.count_of(tid(1)), 1);
        assert_eq!(window.count_of(tid(2)), 0);
        assert_eq!(window.dropped_late(), 1);
    }

    #[test]
    fn leicht_verspaetete_ereignisse_innerhalb_des_fensters_zaehlen_mit() {
        let mut window = SlidingWindow::new(60, 1000);
        window.push(100 * SEC, tid(1));
        // 5 s vor dem neuesten Ereignis, aber klar innerhalb des Fensters.
        window.push(95 * SEC, tid(2));
        assert_eq!(window.count_of(tid(2)), 1);
        assert_eq!(window.dropped_late(), 0);
    }
}
