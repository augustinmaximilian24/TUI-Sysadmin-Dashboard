# Phase 4 – Entwurf: Baselines pro Unit, Zeitprofil, Persistenz

Status: Entwurf (Fable-Anteil). Umsetzung folgt mit Sonnet 5 anhand der
Aufgabenliste in Abschnitt 9. Dieses Dokument ist der Vertrag zwischen
Entwurf und Umsetzung; Abweichungen werden hier nachgezogen, nicht
stillschweigend im Code.

## 1. Ausgangslage und Ziel

Phase 3 vergleicht jedes Template mit seiner eigenen Kurzzeit-Historie
(60 Buckets à 5 s, also 5 Minuten) und bildet das Surprisal gegen die
Verteilung im 60-Sekunden-Momentanfenster. Beides ist bewusst einfach und
hat zwei dokumentierte Grenzen:

1. **Kein Langzeitgedächtnis.** Nach jedem Neustart beginnt die Lernphase
   von vorn. Ein nächtlicher Cron-Job, der einmal am Tag 500 Zeilen
   schreibt, ist für die 5-Minuten-Historie jedes Mal ein Sturm.
2. **Sturmkorrelierte Fehlalarme.** Während ein Template das Momentanfenster
   dominiert, wird jedes andere Template darin relativ selten und schlägt
   im Surprisal aus (siehe `core/tests/anomaly_replay.rs`).

Phase 4 ersetzt den Vergleichsmaßstab: Statt „wie war es in den letzten
fünf Minuten" fragt die Analyse „wie ist es *um diese Tageszeit an dieser
Art von Tag* für *diese Unit* üblicherweise". Dieser Maßstab wird über
Neustarts hinweg gehalten.

Nicht Ziel dieser Phase: Persistenz von Anomalien oder Audit-Log (Phase 8),
Multi-Host (out of scope).

## 2. Begriffe

- **Bucket**: Zeitscheibe fester Länge (`bucket_seconds`, Default 5 s), wie
  in Phase 3.
- **Slot**: Zeitklasse eines Buckets, gebildet aus Stunde (0–23) und Tagesart
  (Werktag/Wochenende). 48 Slots. Begründung in Abschnitt 4.
- **Baseline**: die für einen Schlüssel `(Unit, Template, Slot)` gelernte
  Verteilung der Ereignisse pro Bucket.
- **Unit-Profil**: die langfristige Häufigkeitsverteilung der Templates
  innerhalb einer Unit. Liefert den Nenner für das Surprisal.
- **Vertrauenswürdig** (trusted): eine Baseline mit ausreichend
  Beobachtungsgewicht. Nur vertrauenswürdige Baselines fließen in Scores
  ein; sonst greift die Fallback-Kette (Abschnitt 5).

## 3. Baseline-Semantik

Schlüssel: `(unit_key, template_id, slot)`.

- `unit_key`: FNV-1a-Hash (wie `TemplateId`) über den Unit-Namen; Ereignisse
  ohne `_SYSTEMD_UNIT` bekommen den reservierten Namen `<none>`. Der Klartext
  wird in einer eigenen Tabelle gehalten (Abschnitt 7), damit der Schlüssel
  feste Länge hat.
- Wert: eine **gewichtete Häufigkeitsverteilung der Bucket-Zählwerte**
  (Abschnitt 3.1) plus Metadaten.

Die Rate-Anomalie eines Ereignisses wird gegen die Baseline seines Slots
gebildet: robuster Z-Score (Median/MAD aus der Verteilung, poissonbewusste
Untergrenze `sqrt(median+1)` wie in Phase 3) des laufenden Bucket-Zählwerts.

### 3.1 Repräsentation: gewichtetes Zählwert-Histogramm

Median und MAD lassen sich nicht aus laufenden Summen bilden; man braucht
die Verteilung. Drei Kandidaten wurden abgewogen:

| Variante | Median/MAD | Speicher je Baseline | Vergessen alter Daten |
|---|---|---|---|
| Ring der letzten N Bucket-Zählwerte | exakt | N × 4 B (N=64: 256 B) | hart, nach N Werten |
| P²-Quantilschätzer | näherungsweise | ~80 B | nur über Neuinitialisierung |
| **Gewichtetes Histogramm der Zählwerte** | **exakt** | ~10 Bins × 12 B | **stetig, exponentiell** |

Entscheidung: **gewichtetes Histogramm**. Bucket-Zählwerte sind kleine ganze
Zahlen mit wenigen unterschiedlichen Werten je Slot – ein Histogramm
`Zählwert → Gewicht` ist deshalb kompakt *und* exakt. Gewichte sind `f32`
mit exponentiellem Vergessen (Abschnitt 3.2). Median und MAD werden als
gewichtete Quantile über die sortierten Bins berechnet.

Obergrenzen (Regel 18): höchstens `baseline_max_bins` (Default 32)
unterschiedliche Zählwerte je Histogramm; darüber hinaus wird der höchste
Bin zum Sammelbin. Für Median/MAD ist das unschädlich, solange der Median
klar darunter liegt – und ein Median jenseits von 32 Ereignissen pro 5 s
ist selbst schon eine Rate, bei der die absolute Skala nicht mehr
entscheidend ist.

### 3.2 Vergessen: exponentieller Zerfall, lazy

Jedes Histogramm hält `last_touched_bucket`. Beim nächsten Zugriff werden
alle Gewichte mit `0.5^(vergangene_buckets / halbwert_buckets)`
multipliziert. Halbwertszeit `baseline_half_life_hours` (Default 7 Tage).
Lazy statt periodisch: kein Hintergrundtask, keine Arbeit für Slots, in
denen nichts passiert. Die Summe der Gewichte ist das
**Beobachtungsgewicht** und bestimmt die Vertrauenswürdigkeit.

Bins mit Gewicht unter `1e-3` werden beim Berühren entfernt, sonst bleiben
Leichen einmaliger Ausreißer ewig liegen.

### 3.3 Vertrauenswürdigkeit

Eine Baseline gilt als vertrauenswürdig, wenn ihr Beobachtungsgewicht
`baseline_min_weight` (Default 24, also ~2 Minuten an Buckets in diesem
Slot) erreicht. Da Gewichte zerfallen, kann eine Baseline das Vertrauen
auch wieder verlieren – gewollt: nach drei Wochen ohne Daten in einem Slot
ist die alte Norm nichts mehr wert.

Entscheidend ist, was **nicht** gezählt wird: Buckets, in denen die Unit
*insgesamt* nichts geloggt hat, erzeugen keine Nullbeobachtung für ihre
Templates. Sonst wäre die Baseline eines Templates, das die Unit nur
einmal am Tag schreibt, von 17.000 Nullen dominiert, und der eine
legitime Auftritt ein 50-σ-Ereignis. Nullbeobachtungen entstehen nur für
Buckets, in denen die Unit aktiv war, das Template aber nicht (Umsetzung
in `BaselineStore::close_bucket`, Abschnitt 8).

## 4. Zeitprofil

Slot = `hour_local × 2 + is_weekend`. 48 Slots.

Abgewogen: 24×7 = 168 Slots trennen Montagmorgen von Dienstagmorgen, aber
für einen Heimserver mit spärlichen Daten bedeutet das 168-fache Verdünnung
– die meisten Slots erreichen die Vertrauensschwelle nie. Werktag/Wochenende
trennt die beiden Regime, die sich im Log tatsächlich unterscheiden
(Backups, Desktop-Nutzung, Updates), bei nur zweifacher Verdünnung.
Feiertage werden nicht behandelt (out of scope).

Lokale Zeit statt UTC, weil das Profil menschliches Verhalten abbildet.
Zeitzonen-Konvertierung über die Crate **`chrono`** (Begründung: die
Alternative `time` verweigert die lokale Zeitzone in mehrthreadigen
Prozessen ohne ein explizit als unsound markiertes Feature; `chrono` liest
die System-Zeitzone über die libc sicher). Die Zeitzone wird pro Ereignis
aus dessen Zeitstempel bestimmt, nicht einmalig beim Start – sonst bricht
das Profil an der Sommerzeitumstellung.

Fallback-Kette bei nicht vertrauenswürdigem Slot (Abschnitt 5) verhindert,
dass ein frisch installiertes System wochenlang blind ist.

## 5. Fallback-Kette

Für jedes Ereignis wird der Rate-Z-Score aus der ersten vertrauenswürdigen
Quelle gebildet:

1. Baseline `(unit, template, slot)`
2. Baseline `(unit, template, ANY_SLOT)` – ein 49. Slot, in den *jede*
   Beobachtung zusätzlich einfließt. Kostet ein Histogramm pro
   `(unit, template)`, liefert dafür sofort einen Tagesdurchschnitt.
3. Kurzzeit-Historie aus Phase 3 (`RateTracker`, unverändert)
4. Kein Rate-Signal (0.0)

Das Surprisal wird analog aus dem Unit-Profil gebildet, sofern dessen
Gesamtgewicht `profile_min_weight` erreicht, sonst wie in Phase 3 aus dem
Momentanfenster. Damit ist die sturmkorrelierte Fehlalarmquelle aus Phase 3
gezielt beseitigt: Das Unit-Profil ändert sich in einer Sturm-Minute nur
marginal, weil es mit Halbwertszeit 7 Tage gewichtet ist.

Die `ScoreBreakdown` bekommt ein Feld `rate_source: RateSource`
(`SlotBaseline | AnySlotBaseline | ShortTerm | None`), damit das
Detail-Panel (Phase 7) zeigt, *woran* gemessen wurde.

Die globale Lernphase aus Phase 3 bleibt als Kaltstart-Schutz bestehen,
wird aber übersprungen, wenn beim Start vertrauenswürdige Baselines
geladen wurden (`AnalysisEngine::skip_learning_phase()`).

## 6. Persistenz: redb

### 6.1 Entscheidung

| Kriterium | `redb` | `rusqlite` (bundled) |
|---|---|---|
| Build | reines Rust, kein C-Compiler | bundelt SQLite in C; für den Haswell-Heimserver ggf. Cross-Compile mit C-Toolchain |
| Binärgröße | klein | +~1 MB |
| Zugriffsmuster hier | Key/Value, ganze Datensätze lesen/schreiben – passt | relationale Abfragen – werden nicht gebraucht |
| Transaktionen/Crash-Sicherheit | ACID, Copy-on-Write | ACID, WAL |
| Einzelner Schreiber | Datei-Lock, genau ein Prozess – passt zum Daemon-Modell | mehrere Prozesse möglich – hier nicht nötig |
| Inspektion von außen | keine Standard-CLI | `sqlite3`-CLI |
| Reife | jünger, stabiles Dateiformat seit 1.0 | sehr reif |

Entscheidung: **`redb`**. Ausschlaggebend sind Build-Einfachheit für beide
Zielsysteme und die Passung zum Zugriffsmuster. Der Nachteil fehlender
Inspektion wird durch einen Export ausgeglichen:
`logsentry-daemon --dump-baselines` schreibt den gesamten Bestand als JSON
nach stdout. Das ist für Fehlersuche ohnehin nützlicher als SQL, weil die
Werte Histogramme sind.

### 6.2 Wertformat

Werte werden mit `serde_json` serialisiert. Abgewogen gegen `postcard`
(kompakter, neue Abhängigkeit): Der Bestand liegt in der Größenordnung
weniger Megabyte, wird alle paar Minuten geschrieben und beim Start einmal
gelesen; die Ersparnis rechtfertigt keine zusätzliche Crate. JSON hat den
Vorteil, dass `--dump-baselines` und die Datei dieselbe Darstellung nutzen
und dass additive Schemaänderungen über `#[serde(default)]` ohne Migration
auskommen (Abschnitt 7.2).

Schlüssel sind feste Byte-Arrays (Big-Endian, damit Bereichsscans über
eine Unit möglich bleiben).

### 6.3 Speicherort und Rechte

`/var/lib/logsentry/baselines.redb`, Eigentümer der Daemon-Benutzer,
Modus 0600. Der Daemon ist einziger Schreiber (Regel: Daemon/Client-
Trennung). Die GUI liest Baselines nie direkt; was sie braucht, kommt über
das Protokoll (Phase 6). Pfad in `PersistenceConfig.path`.

### 6.4 Snapshot-Politik

- Alle `snapshot_interval_minutes` (Default 5) und bei sauberem Beenden
  (SIGTERM/SIGINT, Phase 6 verdrahtet das Signal-Handling).
- Ein Snapshot schreibt den **gesamten** Bestand in einer Transaktion. Bei
  Absturz mitten im Schreiben bleibt der vorherige Stand gültig (redb ist
  Copy-on-Write).
- Vor dem Schreiben wird die Obergrenze durchgesetzt: höchstens
  `max_baselines` Einträge (Default 20 000); darüber werden die Einträge mit
  dem geringsten Beobachtungsgewicht verworfen. Bei 20 000 Einträgen à ~300 B
  JSON liegt der Bestand bei ~6 MB – unkritisch für das 50-MB-RSS-Ziel.
- Der Snapshot läuft im Analyse-Task, nicht nebenläufig: Ein Schreiber, kein
  geteilter Zustand. Die Serialisierung von 20 000 Einträgen dauert
  Millisekunden, das Blockieren ist vertretbar. Wird es das nicht mehr, ist
  der Weg ein Klon des Bestands in einen `spawn_blocking`-Task – nicht ein
  zweiter Schreiber.

## 7. Schema

Schema-Version: **1**.

### 7.1 Tabellen

| Tabelle | Schlüssel | Wert |
|---|---|---|
| `meta` | `&str` | `&str` (`schema_version`, `created_us`, `last_snapshot_us`, `hostname`) |
| `units` | `unit_key: u64` | `UnitRecord` JSON: `{ name, first_seen_us, last_seen_us }` |
| `templates` | `template_id: u64` | `TemplateRecord` JSON: `{ template, first_seen_us, last_seen_us, count }` |
| `baselines` | `[u8; 17]` = `unit_key(8) ‖ template_id(8) ‖ slot(1)` | `BaselineRecord` JSON (Abschnitt 8) |
| `unit_profiles` | `unit_key: u64` | `UnitProfileRecord` JSON: `{ counts: Vec<(template_id, weight)>, total_weight, last_touched_bucket }` |
| `silenced` | `template_id: u64` | `SilenceRecord` JSON: `{ until_us: Option<u64> }` – wird in Phase 8 gefüllt, Tabelle wird jetzt angelegt, damit Phase 8 keine Migration braucht |

`hostname` in `meta` dient als Plausibilitätsprüfung: Wird die Datei auf
einen anderen Host kopiert, warnt der Daemon und lernt weiter, statt
schweigend fremde Baselines zu verwenden.

Die `templates`-Tabelle persistiert auch die `TemplateEngine`-Registry, damit
Template-IDs über Neustarts stabil bleiben und das Drain-Clustering seine
verallgemeinerten Templates nicht neu lernen muss.

### 7.2 Migrationspfad

- `meta.schema_version` wird beim Öffnen gelesen.
- Gleich der aktuellen Version: normal laden.
- Kleiner: Migrationen `v1→v2`, `v2→v3`, … laufen der Reihe nach, jede in
  eigener Transaktion, `schema_version` wird nach jeder erhöht. Vor der
  ersten Migration wird die Datei nach `baselines.redb.bak-<version>`
  kopiert.
- Größer (Datei von einer neueren Version): Start wird **verweigert** mit
  klarer Meldung und Hinweis auf `--reset-baselines`. Stillschweigendes
  Überschreiben eines neueren Formats wäre Datenverlust.
- Fehlt `meta` ganz oder ist die Datei nicht lesbar: Meldung, Datei nach
  `baselines.redb.corrupt-<zeitstempel>` verschieben, leer neu beginnen.
  Der Daemon darf wegen einer kaputten Baseline-Datei nicht dauerhaft
  ausfallen; Baselines sind Beschleunigung, keine Voraussetzung.
- `--reset-baselines`: Datei umbenennen (`.bak-manual-<zeitstempel>`), leer
  starten.

Additive Änderungen (neues Feld mit Default) brauchen keine Migration:
alle Datensätze sind `#[serde(default)]`. Eine Migration ist nur nötig,
wenn sich Schlüsselformat oder Bedeutung bestehender Felder ändern.

## 8. Umsetzungsvertrag: Typen und Signaturen

Neues Modul `core::baseline`. Alles außer `store::redb` bleibt frei von
I/O und ist mit festen Fixtures testbar (Regel 24/25).

```rust
// core/src/baseline/slot.rs
/// 0..=47 reguläre Slots, 48 = ANY_SLOT.
pub struct Slot(pub u8);
impl Slot {
    pub const ANY: Slot = Slot(48);
    /// Lokale Zeit aus dem Ereignis-Zeitstempel; Fehler (z. B. Zeitstempel
    /// außerhalb des chrono-Bereichs) fallen auf ANY zurück, nie Panic.
    pub fn from_timestamp_us(timestamp_us: u64) -> Slot;
    pub fn hour(&self) -> Option<u8>;
    pub fn is_weekend(&self) -> Option<bool>;
}

// core/src/baseline/histogram.rs
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CountHistogram {
    /// Sortiert nach Zählwert; letzter Bin ist bei Bedarf Sammelbin.
    bins: Vec<(u32, f32)>,
    last_touched_bucket: u64,
}
impl CountHistogram {
    pub fn observe(&mut self, count: u32, bucket: u64, decay: &DecayParams);
    pub fn total_weight(&self) -> f64;
    /// Gewichteter Median und MAD; None bei Gesamtgewicht 0.
    pub fn median_mad(&self) -> Option<(f64, f64)>;
    pub fn is_trusted(&self, min_weight: f64) -> bool;
}

// core/src/baseline/profile.rs
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UnitProfile {
    counts: Vec<(TemplateId, f32)>,   // begrenzt auf profile_max_templates
    total_weight: f32,
    last_touched_bucket: u64,
}
impl UnitProfile {
    pub fn observe(&mut self, template: TemplateId, bucket: u64, decay: &DecayParams);
    /// Surprisal mit derselben Lidstone-Glättung wie stats::surprisal.
    pub fn surprisal(&self, template: TemplateId, alpha: f64) -> Option<f64>;
    pub fn is_trusted(&self, min_weight: f64) -> bool;
}

// core/src/baseline/store.rs  – reiner In-Memory-Zustand
pub struct BaselineStore { /* HashMap<BaselineKey, CountHistogram>, HashMap<u64, UnitProfile>, offene Bucket-Zähler */ }
pub struct BaselineKey { pub unit_key: u64, pub template_id: TemplateId, pub slot: Slot }
pub enum RateSource { SlotBaseline, AnySlotBaseline, ShortTerm, None }

impl BaselineStore {
    pub fn new(config: &BaselineConfig) -> Self;
    /// Zählt das Ereignis im offenen Bucket; schließt ggf. vorher den alten
    /// Bucket ab (Nullbeobachtungen nur für aktive Units, Abschnitt 3.3).
    pub fn record(&mut self, timestamp_us: u64, unit_key: u64, template: TemplateId);
    /// Rate-Z-Score aus der ersten vertrauenswürdigen Quelle (Stufe 1–2),
    /// None wenn keine Baseline trägt (dann Stufe 3 im Engine).
    pub fn rate_z(&self, timestamp_us: u64, unit_key: u64, template: TemplateId, current_count: u32)
        -> Option<(f64, RateSource)>;
    pub fn surprisal(&self, unit_key: u64, template: TemplateId, alpha: f64) -> Option<f64>;
    pub fn has_trusted_baselines(&self) -> bool;
    /// Erzwingt max_baselines durch Verwerfen der leichtesten Einträge.
    pub fn enforce_limits(&mut self);
    pub fn snapshot(&self) -> BaselineSnapshot;          // serialisierbare Kopie
    pub fn restore(snapshot: BaselineSnapshot, config: &BaselineConfig) -> Self;
}

// core/src/baseline/persist.rs  – redb, einziger I/O-Ort
pub struct BaselineDb { /* redb::Database */ }
impl BaselineDb {
    /// Öffnet oder legt an; führt Migrationen aus; verschiebt defekte Dateien.
    pub fn open(path: &Path, expected_hostname: &str) -> Result<Self, PersistError>;
    pub fn load(&self) -> Result<Option<BaselineSnapshot>, PersistError>;
    pub fn save(&self, snapshot: &BaselineSnapshot, templates: &TemplateSnapshot) -> Result<(), PersistError>;
    pub fn dump_json(&self) -> Result<String, PersistError>;
}
pub const SCHEMA_VERSION: u32 = 1;
```

Änderungen an bestehendem Code:

- `TemplateEngine`: `snapshot() -> TemplateSnapshot` und
  `restore(TemplateSnapshot, …)`. Cluster-IDs, Token-Templates und
  Erstsichtung wandern in die `templates`-Tabelle.
- `AnalysisEngine::process` erhält `unit_key` im `AnalysisInput` (Hash aus
  dem Unit-Namen, im Daemon gebildet) und fragt zuerst den `BaselineStore`
  (Stufe 1–2), dann den `RateTracker` (Stufe 3). `ScoreBreakdown.rate_source`
  wird gesetzt. Surprisal analog.
- `AnalysisConfig` bleibt; neue Sektionen `BaselineConfig` und
  `PersistenceConfig` in der TOML:

```toml
[baseline]
half_life_hours = 168        # 7 Tage
min_weight = 24.0            # Vertrauensschwelle je Slot-Baseline
profile_min_weight = 200.0   # Vertrauensschwelle je Unit-Profil
profile_max_templates = 512  # Obergrenze Templates je Unit-Profil
max_bins = 32                # Obergrenze Bins je Histogramm
max_baselines = 20000        # Obergrenze Einträge insgesamt

[persistence]
path = "/var/lib/logsentry/baselines.redb"
snapshot_interval_minutes = 5
```

- Daemon: `--dump-baselines`, `--reset-baselines`; Snapshot-Timer im
  Analyse-Task; Laden beim Start mit `skip_learning_phase()` bei Erfolg.

## 9. Aufgabenliste für die Umsetzung (Sonnet 5)

Reihenfolge ist verbindlich; jede Zeile = ein Commit, mit
`check → clippy → test` dazwischen (Regel 2/3/6).

1. `chrono` und `redb` in den Workspace aufnehmen; Begründung in den
   Cargo-Kommentar übernehmen (Abschnitt 4 und 6.1).
2. `baseline::slot` mit Tests: Stundenwechsel, Wochenende, Zeitstempel
   außerhalb des gültigen Bereichs → `ANY`, Sommerzeit-Tag (Fixture mit
   fester Zeitzone über `TZ`-Umgebungsvariable im Test).
3. `baseline::histogram` mit Tests: gewichteter Median/MAD gegen bekannte
   Werte, Zerfall über Halbwertszeit, Sammelbin, Bereinigung kleiner
   Gewichte, Vertrauensschwelle.
4. `baseline::profile` mit Tests: Surprisal-Konsistenz zu `stats::surprisal`,
   Obergrenze der Templates (leichteste fliegen raus).
5. `baseline::store` (In-Memory) mit Tests: Nullbeobachtungen nur für aktive
   Units, Fallback-Kette Slot → ANY → None, `enforce_limits`,
   Snapshot/Restore-Roundtrip.
6. `TemplateEngine::snapshot/restore` mit Roundtrip-Test.
7. `AnalysisEngine` anbinden: `unit_key` im Input, `rate_source` im
   Breakdown, Surprisal aus Profil. Replay-Test aus Phase 3 erneut laufen
   lassen: die sturmkorrelierten Fehlalarme müssen verschwinden, die
   Schwelle in `erzeugt_im_normalbetrieb_wenige_fehlalarme` wird auf `== 0`
   verschärft. Bruteforce muss weiterhin bis `critical` eskalieren.
8. `baseline::persist` mit redb: open/load/save/dump, Migrationsgerüst
   (`migrate(from: u32)` mit leerem Match für v1), Hostname-Prüfung,
   Verschieben defekter Dateien. Tests gegen Tempdir: Roundtrip,
   Neuere-Version-wird-verweigert, kaputte Datei wird verschoben.
9. Daemon: Konfigurationssektionen, Laden beim Start, Snapshot-Timer,
   `--dump-baselines`, `--reset-baselines`.
10. Replay-Lauf über die Phase-3-Fixture **zweimal hintereinander** mit
    Persistenz dazwischen: Beim zweiten Lauf darf es keine Lernphase geben
    und die Ergebnisse müssen identisch sein (Regel 28).

## 10. Bewusst offen gelassen

- Feiertage im Zeitprofil.
- Entropie-Baseline je Slot: bleibt vorerst die Kurzzeit-Historie aus
  Phase 3. Der Entropie-Anteil ist nach der Gewichtung mit dem Fensteranteil
  ohnehin klein; ein Slot-Profil dafür lohnt erst, wenn Messdaten aus dem
  Dauerbetrieb zeigen, dass es fehlt.
- Persistenz von Anomalien und Audit-Log (Phase 8). Die `silenced`-Tabelle
  wird jetzt schon angelegt, um Phase 8 eine Migration zu ersparen.

## 11. Nachträge aus der Umsetzung

Abweichungen vom Entwurf, die sich in der Umsetzung als nötig erwiesen
haben. Jede ist im Code an der betreffenden Stelle begründet; hier die
Zusammenfassung.

- **`baseline.min_weight` 24 → 720.** Der Replay-Test (Schritt 7) zeigte,
  dass 24 (~2 Minuten an Buckets) zu niedrig ist: Eine einzelne
  durchgängige Sitzung erreicht den Wert, bevor überhaupt Tag-zu-Tag-
  Streuung beobachtet wurde, und die Baseline erklärt sich auf Basis einer
  untypisch glatten Kurzzeit-Stichprobe selbst für vertrauenswürdig.
  720 ist weiterhin nur ein Näherungswert für "über mehrere Tage
  beobachtet"; das Gewicht unterscheidet nicht zwischen einer langen
  Sitzung und mehreren Besuchen an verschiedenen Tagen. Genauere Lösung
  (Verfolgung unterschiedlicher Kalendertage je Slot) bleibt offen.
- **Signaturen:** `HistogramConfig`/`ProfileConfig` bündeln Zerfall und
  Obergrenze; `BaselineStore::new/restore` erhalten `bucket_seconds`
  zusätzlich, da `BaselineConfig` den Wert bewusst nicht dupliziert.
- **`TemplateRecord.order`:** Ein Key/Value-Speicher liefert die
  Template-Registry nach ID sortiert, nicht in Registry-Reihenfolge. Da
  die Reihenfolge bei Ähnlichkeitsgleichstand das Matching bestimmt, wird
  die Position explizit mitgesichert (Schritt 10 deckte das auf).
- **`units`-Tabelle** wird angelegt, aber noch nicht befüllt: Der
  `BaselineStore` kennt nur Unit-Hashes. Das Durchreichen der Klartextnamen
  aus dem Daemon ist eine Ergänzung für Phase 6/7 (Anzeige in der GUI).
- **Persistenzfehler beim Start:** Nur eine Datei *neuerer* Schema-Version
  verhindert den Start. Alle anderen Fehler (Rechte, Pfad) führen zum
  Betrieb ohne Persistenz mit Warnung -- Baselines sind Beschleunigung,
  keine Voraussetzung.
- **Verbleibender Fehlalarm im Replay-Korpus:** Von ursprünglich drei
  (nicht zwei, wie in der Phase-3-Zusammenfassung angegeben) bleibt einer
  (t=304 s). Er läuft vollständig über die Kurzzeit-Pfade aus Phase 3
  (Kaltstart-Artefakt der 300-Sekunden-Rate-Historie) und ist keine Lücke
  der Baseline-Logik. Die beiden sturmkorrelierten sind verschwunden; ein
  Regressionstest sichert das ab.
