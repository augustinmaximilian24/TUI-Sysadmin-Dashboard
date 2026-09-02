//! Template-Extraktion: ordnet maskierten Log-Zeilen eine stabile
//! Template-ID zu und verallgemeinert Zeilen, die die Regex-Masken aus
//! [`crate::mask`] nicht abdecken (z. B. wechselnde Benutzernamen), über ein
//! einfaches Drain-artiges Ähnlichkeits-Clustering.
//!
//! Funktionsweise: Nach der Regex-Maskierung wird die Token-Sequenz mit
//! bestehenden Clustern gleicher Länge verglichen. Stimmt der Anteil
//! übereinstimmender Tokens mit dem besten Cluster über einem konfigurierten
//! Schwellwert, wird die Zeile diesem Cluster zugeordnet und abweichende
//! Tokens im Cluster-Template auf `<*>` verallgemeinert. Andernfalls entsteht
//! ein neues Cluster. Die Template-ID wird einmalig bei Cluster-Erstellung
//! aus der ursprünglichen Token-Sequenz gehasht und bleibt danach stabil,
//! auch wenn sich das Cluster-Template durch Verallgemeinerung weiterentwickelt.

use std::collections::HashMap;

use crate::mask::mask_message;

/// Stabile Kennung eines Templates (FNV-1a-Hash über die Ursprungs-Tokens).
///
/// Zwei reservierte Werte: [`TemplateId::EMPTY`] für leere Nachrichten und
/// [`TemplateId::OVERFLOW`], wenn die Registry ihre Kapazitätsgrenze
/// erreicht hat (Regel 18: harte Obergrenze statt unbeschränktem Wachstum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TemplateId(pub u64);

impl TemplateId {
    /// Reservierte ID für leere Nachrichten (kein Token vorhanden).
    pub const EMPTY: TemplateId = TemplateId(0);
    /// Reservierte ID, wenn die Template-Registry voll ist und keine neuen
    /// Cluster mehr angelegt werden.
    pub const OVERFLOW: TemplateId = TemplateId(u64::MAX);
}

impl std::fmt::Display for TemplateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Berechnet einen stabilen 64-Bit-Hash (FNV-1a) über die gegebenen Bytes.
///
/// Bewusst keine Nutzung von `std::collections::hash_map::DefaultHasher`:
/// dessen Algorithmus ist laut Std-Doku *nicht* über Rust-Versionen hinweg
/// stabil, wir brauchen aber über Neustarts/Rust-Updates hinweg reproduzierbare
/// IDs (Regel: „Template-ID über stabilen Hash“).
fn fnv1a_hash64(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Platzhalter für eine Token-Position, die sich zwischen mehreren Zeilen
/// desselben Clusters unterscheidet.
const WILDCARD: &str = "<*>";

/// Ein Cluster im Drain-artigen Modell: eine Gruppe strukturell ähnlicher
/// Zeilen gleicher (maskierter) Token-Länge.
#[derive(Debug, Clone)]
struct Cluster {
    id: TemplateId,
    token_template: Vec<String>,
    first_seen_us: u64,
    last_seen_us: u64,
    count: u64,
}

/// Ergebnis der Verarbeitung einer Nachricht: welchem Template sie
/// zugeordnet wurde und ob dafür ein neues Cluster angelegt werden musste.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateMatch {
    /// Stabile Template-ID.
    pub id: TemplateId,
    /// Aktuelle (ggf. verallgemeinerte) Template-Darstellung, Tokens durch
    /// Leerzeichen getrennt.
    pub template: String,
    /// Zeitpunkt der ersten Sichtung dieses Templates (Mikrosekunden seit Epoch).
    pub first_seen_us: u64,
    /// Anzahl bisher diesem Template zugeordneter Zeilen (inklusive dieser).
    pub count: u64,
    /// War dies die erste Sichtung (neues Cluster angelegt)?
    pub is_new: bool,
}

/// Zustand der Template-Extraktion: verwaltet alle Cluster und ordnet neue
/// Zeilen zu. Nicht `Clone`, da die Registry über die Laufzeit des Daemons
/// hinweg einen einzigen, wachsenden Zustand darstellt.
pub struct TemplateEngine {
    clusters: Vec<Cluster>,
    /// Index: Token-Länge -> Indizes in `clusters` mit dieser Länge.
    /// Vermeidet, bei jeder Zeile alle Cluster zu durchsuchen.
    by_len: HashMap<usize, Vec<usize>>,
    similarity_threshold: f64,
    max_templates: usize,
    /// Anzahl Zeilen, die wegen erreichter `max_templates`-Grenze keinem
    /// neuen Cluster zugeordnet werden konnten (sichtbar für die GUI).
    overflow_count: u64,
}

impl TemplateEngine {
    /// Erstellt eine neue, leere Template-Engine.
    ///
    /// `similarity_threshold` sollte zwischen 0.0 und 1.0 liegen; Werte
    /// außerhalb werden auf diesen Bereich begrenzt.
    pub fn new(similarity_threshold: f64, max_templates: usize) -> Self {
        Self {
            clusters: Vec::new(),
            by_len: HashMap::new(),
            similarity_threshold: similarity_threshold.clamp(0.0, 1.0),
            max_templates,
            overflow_count: 0,
        }
    }

    /// Anzahl der Zeilen, die wegen voller Registry keinem neuen Template
    /// zugeordnet werden konnten.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count
    }

    /// Anzahl aktuell verwalteter Templates.
    pub fn template_count(&self) -> usize {
        self.clusters.len()
    }

    /// Verarbeitet eine Log-Nachricht: maskiert sie, ordnet sie einem
    /// bestehenden Cluster zu oder legt (sofern Kapazität vorhanden) ein
    /// neues an.
    pub fn process(&mut self, message: &str, timestamp_us: u64) -> TemplateMatch {
        let tokens = mask_message(message);

        if tokens.is_empty() {
            return TemplateMatch {
                id: TemplateId::EMPTY,
                template: String::new(),
                first_seen_us: timestamp_us,
                count: 0,
                is_new: false,
            };
        }

        if let Some(cluster_idx) = self.find_best_cluster(&tokens) {
            let cluster = &mut self.clusters[cluster_idx];
            merge_into_template(&mut cluster.token_template, &tokens);
            cluster.count += 1;
            cluster.last_seen_us = timestamp_us;
            return TemplateMatch {
                id: cluster.id,
                template: cluster.token_template.join(" "),
                first_seen_us: cluster.first_seen_us,
                count: cluster.count,
                is_new: false,
            };
        }

        if self.clusters.len() >= self.max_templates {
            self.overflow_count += 1;
            tracing::warn!(
                max_templates = self.max_templates,
                "Template-Registry voll, Zeile wird nicht als neues Template registriert"
            );
            return TemplateMatch {
                id: TemplateId::OVERFLOW,
                template: tokens.join(" "),
                first_seen_us: timestamp_us,
                count: 0,
                is_new: false,
            };
        }

        let id = TemplateId(fnv1a_hash64(tokens.join(" ").as_bytes()));
        let template_str = tokens.join(" ");
        let len = tokens.len();
        let new_index = self.clusters.len();
        self.clusters.push(Cluster {
            id,
            token_template: tokens,
            first_seen_us: timestamp_us,
            last_seen_us: timestamp_us,
            count: 1,
        });
        self.by_len.entry(len).or_default().push(new_index);

        TemplateMatch {
            id,
            template: template_str,
            first_seen_us: timestamp_us,
            count: 1,
            is_new: true,
        }
    }

    /// Sucht unter allen Clustern mit passender Token-Länge das ähnlichste,
    /// sofern es über der konfigurierten Schwelle liegt.
    fn find_best_cluster(&self, tokens: &[String]) -> Option<usize> {
        let candidates = self.by_len.get(&tokens.len())?;
        let mut best: Option<(usize, f64)> = None;

        for &idx in candidates {
            let similarity = token_similarity(&self.clusters[idx].token_template, tokens);
            if similarity >= self.similarity_threshold
                && best.is_none_or(|(_, best_sim)| similarity > best_sim)
            {
                best = Some((idx, similarity));
            }
        }

        best.map(|(idx, _)| idx)
    }
}

/// Anteil der Positionen, an denen `template` entweder bereits ein Wildcard
/// ist oder exakt mit `tokens` übereinstimmt. Erwartet gleiche Länge.
fn token_similarity(template: &[String], tokens: &[String]) -> f64 {
    debug_assert_eq!(template.len(), tokens.len());
    if template.is_empty() {
        return 1.0;
    }
    let matching = template
        .iter()
        .zip(tokens.iter())
        .filter(|(t, tok)| t.as_str() == WILDCARD || t == tok)
        .count();
    matching as f64 / template.len() as f64
}

/// Verallgemeinert `template` an allen Positionen, an denen es von `tokens`
/// abweicht, zu `<*>`.
fn merge_into_template(template: &mut [String], tokens: &[String]) {
    for (slot, tok) in template.iter_mut().zip(tokens.iter()) {
        if slot != WILDCARD && slot != tok {
            *slot = WILDCARD.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identische_nachrichten_ergeben_gleiche_id_und_zaehlen_hoch() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let first = engine.process("Accepted publickey for admin from 10.0.0.5", 1000);
        let second = engine.process("Accepted publickey for admin from 10.0.0.5", 2000);

        assert_eq!(first.id, second.id);
        assert!(first.is_new);
        assert!(!second.is_new);
        assert_eq!(second.count, 2);
        assert_eq!(second.first_seen_us, 1000);
    }

    #[test]
    fn abweichende_zahlen_werden_von_masken_bereits_vereinheitlicht() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let a = engine.process("Failed password for root from 203.0.113.9 port 51422 ssh2", 1000);
        let b = engine.process("Failed password for root from 203.0.113.9 port 51423 ssh2", 1001);
        assert_eq!(a.id, b.id, "Ports/IPs werden schon durch Masken vereinheitlicht");
    }

    #[test]
    fn abweichender_freitext_wird_durch_drain_clustering_verallgemeinert() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let a = engine.process("Started backup job for user alice", 1000);
        let b = engine.process("Started backup job for user bob", 1001);
        assert_eq!(a.id, b.id, "einzelnes abweichendes Token sollte gemergt werden");
        assert!(b.template.contains(WILDCARD));
    }

    #[test]
    fn voellig_unterschiedliche_nachrichten_bekommen_verschiedene_ids() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let a = engine.process("Accepted publickey for admin from 10.0.0.5", 1000);
        let b = engine.process("Killed process 5321 due to memory pressure", 1001);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn zu_viele_abweichende_tokens_bleiben_getrennte_cluster() {
        // Bei 5 Tokens mit Schwelle 0.7 muss Ähnlichkeit >= 0.7 sein (>=4/5).
        // Zwei abweichende von fünf Tokens (3/5 = 0.6) liegen darunter.
        let mut engine = TemplateEngine::new(0.7, 100);
        let a = engine.process("alpha beta gamma delta epsilon", 1000);
        let b = engine.process("wxyz qrst gamma delta epsilon", 1001);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn leere_nachricht_ergibt_reservierte_empty_id() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let result = engine.process("", 1000);
        assert_eq!(result.id, TemplateId::EMPTY);
    }

    #[test]
    fn max_templates_grenze_wird_respektiert() {
        let mut engine = TemplateEngine::new(0.7, 2);
        let a = engine.process("erste ganz eigene nachricht", 1000);
        let b = engine.process("zweite komplett andere sache", 1001);
        let c = engine.process("dritte voellig verschiedene zeile", 1002);

        assert!(a.is_new);
        assert!(b.is_new);
        assert_eq!(c.id, TemplateId::OVERFLOW);
        assert_eq!(engine.overflow_count(), 1);
        assert_eq!(engine.template_count(), 2);
    }

    #[test]
    fn template_id_ist_ueber_mehrere_engine_instanzen_stabil() {
        // Gleiche Eingabe -> gleiche ID, auch in einer frischen Engine
        // (bestätigt: ID hängt nur vom Hash ab, nicht von internem Zustand).
        let mut engine_a = TemplateEngine::new(0.7, 100);
        let mut engine_b = TemplateEngine::new(0.7, 100);
        let a = engine_a.process("Accepted publickey for admin from 10.0.0.5", 1000);
        let b = engine_b.process("Accepted publickey for admin from 10.0.0.5", 9999);
        assert_eq!(a.id, b.id);
    }
}
