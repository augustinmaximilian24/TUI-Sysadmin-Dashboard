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

use serde::{Deserialize, Serialize};

use crate::mask::mask_message;

/// Stabile Kennung eines Templates (FNV-1a-Hash über die Ursprungs-Tokens).
///
/// Zwei reservierte Werte: [`TemplateId::EMPTY`] für leere Nachrichten und
/// [`TemplateId::OVERFLOW`], wenn die Registry ihre Kapazitätsgrenze
/// erreicht hat (Regel 18: harte Obergrenze statt unbeschränktem Wachstum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
/// Delegiert an [`crate::hash::fnv1a_hash64`] (gemeinsam mit den
/// Unit-Schlüsseln in [`crate::baseline`] genutzt); Begründung für die
/// Wahl von FNV-1a statt `DefaultHasher` dort.
fn fnv1a_hash64(data: &[u8]) -> u64 {
    crate::hash::fnv1a_hash64(data)
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

/// Serialisierbare Darstellung eines Clusters für die Persistenz.
///
/// Enthält bewusst die **Token-Liste** statt nur des zusammengefügten
/// Template-Strings: Nur so lässt sich das Drain-Clustering nach einem
/// Neustart exakt fortsetzen, ohne dass Wildcard-Positionen erneut gelernt
/// werden müssen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TemplateRecord {
    /// Stabile Template-ID (bleibt über Neustarts erhalten).
    pub id: TemplateId,
    /// Position in der Registry. Wird beim Wiederherstellen zur Sortierung
    /// genutzt, weil ein Key/Value-Speicher die Einträge nach Schlüssel
    /// (Template-ID) liefert, nicht in Einfügereihenfolge -- die Reihenfolge
    /// ist aber Teil des beobachtbaren Verhaltens (siehe
    /// [`TemplateSnapshot`]). `#[serde(default)]` hält ältere Bestände ohne
    /// dieses Feld lesbar.
    pub order: u64,
    /// Token-Template inklusive `<*>`-Wildcards.
    pub tokens: Vec<String>,
    /// Zeitpunkt der ersten Sichtung (Mikrosekunden seit Epoch).
    pub first_seen_us: u64,
    /// Zeitpunkt der letzten Sichtung (Mikrosekunden seit Epoch).
    pub last_seen_us: u64,
    /// Anzahl bisher zugeordneter Zeilen.
    pub count: u64,
}

impl Default for TemplateRecord {
    fn default() -> Self {
        Self {
            id: TemplateId::EMPTY,
            order: 0,
            tokens: Vec::new(),
            first_seen_us: 0,
            last_seen_us: 0,
            count: 0,
        }
    }
}

/// Serialisierbarer Schnappschuss der gesamten Template-Registry.
///
/// Die Reihenfolge der Cluster wird erhalten: Bei gleicher Ähnlichkeit
/// gewinnt in [`TemplateEngine::find_best_cluster`] der zuerst gefundene
/// Cluster, die Reihenfolge ist also Teil des beobachtbaren Verhaltens.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct TemplateSnapshot {
    /// Alle Cluster in ihrer internen Reihenfolge.
    pub clusters: Vec<TemplateRecord>,
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

    /// Serialisierbare Kopie der gesamten Registry (für die Persistenz,
    /// Phase 4). Der `overflow_count` wird nicht mitgesichert: Er ist eine
    /// Laufzeit-Kennzahl der aktuellen Sitzung, keine gelernte Information.
    pub fn snapshot(&self) -> TemplateSnapshot {
        TemplateSnapshot {
            clusters: self
                .clusters
                .iter()
                .enumerate()
                .map(|(order, cluster)| TemplateRecord {
                    id: cluster.id,
                    order: order as u64,
                    tokens: cluster.token_template.clone(),
                    first_seen_us: cluster.first_seen_us,
                    last_seen_us: cluster.last_seen_us,
                    count: cluster.count,
                })
                .collect(),
        }
    }

    /// Baut eine Engine aus einem Schnappschuss wieder auf.
    ///
    /// Enthält der Schnappschuss mehr Cluster, als `max_templates` erlaubt
    /// (z. B. nachdem die Grenze in der Konfiguration gesenkt wurde), werden
    /// die überzähligen am Ende verworfen -- das sind bei erhaltener
    /// Reihenfolge die zuletzt angelegten, also tendenziell jüngsten und
    /// am wenigsten bestätigten Cluster. Leere Token-Listen werden
    /// übersprungen, da sie keinem gültigen Cluster entsprechen.
    pub fn restore(
        snapshot: TemplateSnapshot,
        similarity_threshold: f64,
        max_templates: usize,
    ) -> Self {
        let mut engine = Self::new(similarity_threshold, max_templates);
        // Stabil nach `order` sortieren: Bestände aus einem Key/Value-Speicher
        // kommen nach Template-ID geordnet an, nicht in Registry-Reihenfolge.
        let mut clusters = snapshot.clusters;
        clusters.sort_by_key(|record| record.order);
        for record in clusters.into_iter().take(max_templates) {
            if record.tokens.is_empty() {
                continue;
            }
            let len = record.tokens.len();
            let index = engine.clusters.len();
            engine.clusters.push(Cluster {
                id: record.id,
                token_template: record.tokens,
                first_seen_us: record.first_seen_us,
                last_seen_us: record.last_seen_us,
                count: record.count,
            });
            engine.by_len.entry(len).or_default().push(index);
        }
        engine
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

/// Anteil der noch konkreten (nicht bereits zu `<*>` verallgemeinerten)
/// Positionen von `template`, an denen `tokens` exakt übereinstimmt.
/// Erwartet gleiche Länge.
///
/// Bereits generalisierte Positionen tragen bewusst weder zum Zähler noch
/// zum Nenner bei: Würden sie (wie in einer früheren Version) automatisch
/// als Treffer gezählt, zöge ein Cluster, das erst einmal an mehreren
/// Positionen zu `<*>` verallgemeinert wurde, praktisch jede weitere Zeile
/// gleicher Tokenlänge an -- mit jeder weiteren Verallgemeinerung würde das
/// noch wahrscheinlicher, bis das Cluster irreversibel zu einem
/// Alles-Wildcard-Template kollabiert und für diese Zeilenlänge nie wieder
/// ein neues Template (und damit nie wieder Surprisal) entstehen kann. Mit
/// nur noch konkreten Positionen im Nenner sinkt die verbleibende
/// Toleranz dagegen mit jeder Verallgemeinerung, und ein bereits
/// vollständig generalisiertes Template (keine konkreten Positionen mehr)
/// gilt als nicht mehr ähnlich zu irgendetwas.
fn token_similarity(template: &[String], tokens: &[String]) -> f64 {
    debug_assert_eq!(template.len(), tokens.len());
    if template.is_empty() {
        return 1.0;
    }
    let mut concrete = 0usize;
    let mut concrete_matches = 0usize;
    for (t, tok) in template.iter().zip(tokens.iter()) {
        if t.as_str() == WILDCARD {
            continue;
        }
        concrete += 1;
        if t == tok {
            concrete_matches += 1;
        }
    }
    if concrete == 0 {
        return 0.0;
    }
    concrete_matches as f64 / concrete as f64
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
        let a = engine.process(
            "Failed password for root from 203.0.113.9 port 51422 ssh2",
            1000,
        );
        let b = engine.process(
            "Failed password for root from 203.0.113.9 port 51423 ssh2",
            1001,
        );
        assert_eq!(
            a.id, b.id,
            "Ports/IPs werden schon durch Masken vereinheitlicht"
        );
    }

    #[test]
    fn abweichender_freitext_wird_durch_drain_clustering_verallgemeinert() {
        let mut engine = TemplateEngine::new(0.7, 100);
        let a = engine.process("Started backup job for user alice", 1000);
        let b = engine.process("Started backup job for user bob", 1001);
        assert_eq!(
            a.id, b.id,
            "einzelnes abweichendes Token sollte gemergt werden"
        );
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

    #[test]
    fn snapshot_restore_roundtrip_ueber_json() {
        let mut engine = TemplateEngine::new(0.7, 100);
        engine.process("Started backup job for user alice", 1000);
        engine.process("Started backup job for user bob", 1001);
        engine.process("Accepted publickey for admin from 10.0.0.5", 1002);
        let vorher_count = engine.template_count();

        let snapshot = engine.snapshot();
        let json = serde_json::to_string(&snapshot).expect("serialisierbar");
        let restored_snapshot: TemplateSnapshot =
            serde_json::from_str(&json).expect("deserialisierbar");
        assert_eq!(
            restored_snapshot, snapshot,
            "JSON-Roundtrip muss verlustfrei sein"
        );

        let mut restored = TemplateEngine::restore(restored_snapshot, 0.7, 100);
        assert_eq!(restored.template_count(), vorher_count);

        // Das verallgemeinerte Wildcard-Template muss erhalten sein: ein
        // dritter Name landet ohne Neulernen im bestehenden Cluster.
        let original_id = engine
            .process("Started backup job for user charlie", 2000)
            .id;
        let restored_id = restored
            .process("Started backup job for user charlie", 2000)
            .id;
        assert_eq!(original_id, restored_id);
        assert!(
            !restored
                .process("Started backup job for user dave", 2001)
                .is_new
        );
    }

    #[test]
    fn restore_erhaelt_erstsichtung_und_zaehler() {
        let mut engine = TemplateEngine::new(0.7, 100);
        engine.process("Accepted publickey for admin from 10.0.0.5", 1000);
        engine.process("Accepted publickey for admin from 10.0.0.5", 2000);

        let mut restored = TemplateEngine::restore(engine.snapshot(), 0.7, 100);
        let m = restored.process("Accepted publickey for admin from 10.0.0.5", 3000);
        assert_eq!(
            m.first_seen_us, 1000,
            "Erstsichtung darf beim Neustart nicht verloren gehen"
        );
        assert_eq!(
            m.count, 3,
            "Zähler muss über den Neustart hinweg weiterzählen"
        );
        assert!(!m.is_new);
    }

    #[test]
    fn restore_respektiert_gesenkte_max_templates_grenze() {
        let mut engine = TemplateEngine::new(0.7, 100);
        engine.process("erste ganz eigene nachricht", 1);
        engine.process("zweite komplett andere sache", 2);
        engine.process("dritte voellig verschiedene zeile", 3);
        assert_eq!(engine.template_count(), 3);

        let restored = TemplateEngine::restore(engine.snapshot(), 0.7, 2);
        assert_eq!(
            restored.template_count(),
            2,
            "Obergrenze muss beim Wiederherstellen greifen"
        );
    }

    #[test]
    fn restore_stellt_registry_reihenfolge_ueber_order_wieder_her() {
        // Ein Key/Value-Speicher liefert nach ID sortiert; die Registry-
        // Reihenfolge muss trotzdem aus `order` zurückkommen.
        let mut engine = TemplateEngine::new(0.7, 100);
        engine.process("erste ganz eigene nachricht", 1);
        engine.process("zweite komplett andere sache", 2);
        engine.process("dritte voellig verschiedene zeile", 3);
        let original = engine.snapshot();

        let mut verwuerfelt = original.clone();
        verwuerfelt.clusters.sort_by_key(|r| r.id.0);
        assert_ne!(
            verwuerfelt
                .clusters
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            original.clusters.iter().map(|r| r.id).collect::<Vec<_>>(),
            "Testvoraussetzung: ID-Sortierung muss die Reihenfolge tatsächlich ändern"
        );

        let restored = TemplateEngine::restore(verwuerfelt, 0.7, 100);
        assert_eq!(restored.snapshot(), original);
    }

    #[test]
    fn restore_ueberspringt_leere_token_listen() {
        let snapshot = TemplateSnapshot {
            clusters: vec![
                TemplateRecord {
                    tokens: Vec::new(),
                    ..TemplateRecord::default()
                },
                TemplateRecord {
                    id: TemplateId(42),
                    tokens: vec!["a".to_string(), "b".to_string()],
                    ..TemplateRecord::default()
                },
            ],
        };
        let restored = TemplateEngine::restore(snapshot, 0.7, 100);
        assert_eq!(restored.template_count(), 1);
    }

    #[test]
    fn kollabierendes_cluster_wird_durch_sinkende_konkrete_basis_gestoppt() {
        // Regression: token_similarity zählte bereits generalisierte
        // Wildcard-Positionen automatisch als Treffer. Dadurch zog ein
        // Cluster, das schon an mehreren Positionen verallgemeinert war,
        // praktisch jede weitere Zeile gleicher Tokenlänge an und
        // kollabierte irreversibel zu einem Alles-Wildcard-Template --
        // Surprisal für diese Zeilenlänge starb damit dauerhaft.
        let mut engine = TemplateEngine::new(0.7, 100);
        let basis = engine.process("alpha beta gamma delta epsilon", 0);
        engine.process("zulu beta gamma delta epsilon", 1);
        engine.process("zulu yankee gamma delta epsilon", 2);
        // Die dritte Abweichung liegt jetzt unter der Schwelle (die
        // verbleibende konkrete Basis ist zu klein geworden) -- es entsteht
        // ein neues Cluster statt weiterer Verallgemeinerung.
        let dritte = engine.process("zulu yankee xray delta epsilon", 3);
        assert_ne!(
            dritte.id, basis.id,
            "die Verallgemeinerung darf nicht bis zur dritten Abweichung fortschreiten"
        );
        assert_eq!(engine.template_count(), 2);

        // Eine völlig unbeteiligte Zeile gleicher Länge darf das inzwischen
        // teilweise generalisierte Ursprungscluster nicht treffen.
        let unbeteiligt = engine.process("kernel oom killed process foo", 4);
        assert!(
            unbeteiligt.is_new,
            "ein degeneriertes Cluster darf nicht jede Zeile gleicher Länge anziehen"
        );
    }

    #[test]
    fn leerer_snapshot_ergibt_leere_engine() {
        let restored = TemplateEngine::restore(TemplateSnapshot::default(), 0.7, 100);
        assert_eq!(restored.template_count(), 0);
    }
}
