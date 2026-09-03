//! Persistenz der Baselines und der Template-Registry mit `redb`.
//!
//! Einziger Ort mit Datei-I/O im `baseline`-Modul. Schema, Snapshot-Politik
//! und Migrationspfad: `docs/phase4-baselines.md` Abschnitte 6 und 7.
//!
//! Werte werden als JSON-Strings abgelegt (Begründung in Abschnitt 6.2:
//! additive Schemaänderungen brauchen dank `#[serde(default)]` keine
//! Migration, und `--dump-baselines` nutzt dieselbe Darstellung).
//!
//! Abweichung vom Entwurf: Die Tabelle `units` (Klartext der Unit-Namen)
//! wird angelegt, aber noch nicht befüllt -- der `BaselineStore` kennt nur
//! Unit-Hashes, und das Durchreichen der Namen aus dem Daemon ist Teil von
//! Schritt 9. Die Tabelle jetzt schon anzulegen erspart dann eine
//! Migration, genau wie bei `silenced` für Phase 8.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::template::{TemplateId, TemplateSnapshot};

use super::histogram::CountHistogram;
use super::profile::UnitProfile;
use super::slot::Slot;
use super::store::{BaselineKey, BaselineSnapshot};

/// Aktuelle Schema-Version. Wird in `meta.schema_version` abgelegt.
pub const SCHEMA_VERSION: u32 = 1;

const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
const UNITS: TableDefinition<u64, &str> = TableDefinition::new("units");
const TEMPLATES: TableDefinition<u64, &str> = TableDefinition::new("templates");
const BASELINES: TableDefinition<[u8; 17], &str> = TableDefinition::new("baselines");
const UNIT_PROFILES: TableDefinition<u64, &str> = TableDefinition::new("unit_profiles");
const SILENCED: TableDefinition<u64, &str> = TableDefinition::new("silenced");

const META_SCHEMA_VERSION: &str = "schema_version";
const META_HOSTNAME: &str = "hostname";
const META_CREATED_US: &str = "created_us";
const META_LAST_SNAPSHOT_US: &str = "last_snapshot_us";

/// Fehler der Persistenzschicht.
#[derive(Debug, Error)]
pub enum PersistError {
    /// Fehler aus `redb` (Öffnen, Transaktion, Tabelle, Commit, …).
    /// Geboxt, weil `redb::Error` über 160 Bytes groß ist und sonst jeden
    /// `Result` dieser Schicht entsprechend aufblähen würde.
    #[error("Datenbankfehler: {0}")]
    Db(#[source] Box<redb::Error>),

    /// Datei-Operation außerhalb von `redb` (Sichern, Verschieben) schlug fehl.
    #[error("Dateioperation fehlgeschlagen: {0}")]
    Io(#[from] std::io::Error),

    /// Ein gespeicherter Wert ließ sich nicht (de)serialisieren.
    #[error("Serialisierungsfehler: {0}")]
    Json(#[from] serde_json::Error),

    /// Die Datei stammt von einer neueren Programmversion. Wird bewusst
    /// nicht stillschweigend überschrieben (das wäre Datenverlust).
    #[error(
        "Baseline-Datei hat Schema-Version {found}, dieses Programm unterstützt \
         höchstens {supported}. Lösung: neuere Version verwenden oder mit \
         --reset-baselines neu beginnen (die alte Datei wird dabei gesichert)."
    )]
    NewerSchema {
        /// In der Datei gefundene Version.
        found: u32,
        /// Höchste von diesem Programm unterstützte Version.
        supported: u32,
    },

    /// Für diese Ausgangsversion existiert keine Migration.
    #[error("keine Migration von Schema-Version {0} bekannt")]
    NoMigrationPath(u32),

    /// `meta` ist vorhanden, aber inhaltlich unbrauchbar.
    #[error("meta-Tabelle unbrauchbar: {0}")]
    InvalidMeta(String),
}

impl From<redb::Error> for PersistError {
    fn from(err: redb::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

/// Alles, was beim Start aus der Datei wiederhergestellt wird.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// Slot-Histogramme und Unit-Profile.
    pub baselines: BaselineSnapshot,
    /// Template-Registry (Drain-Cluster).
    pub templates: TemplateSnapshot,
}

/// Vollständiger, menschenlesbarer Export (`--dump-baselines`).
#[derive(Debug, Serialize)]
struct Dump<'a> {
    schema_version: u32,
    hostname: Option<String>,
    created_us: Option<u64>,
    last_snapshot_us: Option<u64>,
    state: &'a PersistedState,
}

/// Geöffnete Baseline-Datenbank. Der Daemon ist einziger Schreiber
/// (`redb` erzwingt das über einen Datei-Lock).
#[derive(Debug)]
pub struct BaselineDb {
    db: Database,
    path: PathBuf,
}

impl BaselineDb {
    /// Öffnet die Datei oder legt sie an. Führt ausstehende Migrationen aus,
    /// verweigert Dateien neuerer Versionen und verschiebt unlesbare Dateien
    /// zur Seite (`<pfad>.corrupt-<zeitstempel>`), statt den Daemon dauerhaft
    /// am Start zu hindern -- Baselines sind Beschleunigung, keine
    /// Voraussetzung.
    ///
    /// Weicht der gespeicherte Hostname von `expected_hostname` ab, wird
    /// gewarnt und weitergelernt (die Datei wurde vermutlich kopiert).
    pub fn open(path: &Path, expected_hostname: &str) -> Result<Self, PersistError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let db = match Database::create(path) {
            Ok(db) => db,
            Err(err) => {
                let moved = move_aside(path, "corrupt")?;
                tracing::warn!(
                    pfad = %path.display(),
                    verschoben_nach = %moved.display(),
                    fehler = %err,
                    "Baseline-Datei unlesbar, beginne mit leerer Datenbank"
                );
                Database::create(path).map_err(redb::Error::from)?
            }
        };

        let mut this = Self {
            db,
            path: path.to_path_buf(),
        };
        this.ensure_tables()?;

        match this.read_schema_version()? {
            None => this.write_fresh_meta(expected_hostname)?,
            Some(found) if found == SCHEMA_VERSION => {}
            Some(found) if found > SCHEMA_VERSION => {
                return Err(PersistError::NewerSchema {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
            Some(found) => this.migrate_from(found)?,
        }

        this.check_hostname(expected_hostname)?;
        Ok(this)
    }

    /// Legt alle Tabellen an, falls sie fehlen (idempotent).
    fn ensure_tables(&self) -> Result<(), PersistError> {
        let txn = self.db.begin_write().map_err(redb::Error::from)?;
        {
            txn.open_table(META).map_err(redb::Error::from)?;
            txn.open_table(UNITS).map_err(redb::Error::from)?;
            txn.open_table(TEMPLATES).map_err(redb::Error::from)?;
            txn.open_table(BASELINES).map_err(redb::Error::from)?;
            txn.open_table(UNIT_PROFILES).map_err(redb::Error::from)?;
            txn.open_table(SILENCED).map_err(redb::Error::from)?;
        }
        txn.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    fn read_meta(&self, key: &str) -> Result<Option<String>, PersistError> {
        let txn = self.db.begin_read().map_err(redb::Error::from)?;
        let table = txn.open_table(META).map_err(redb::Error::from)?;
        let value = table
            .get(key)
            .map_err(redb::Error::from)?
            .map(|guard| guard.value().to_string());
        Ok(value)
    }

    fn write_meta(&self, key: &str, value: &str) -> Result<(), PersistError> {
        let txn = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut table = txn.open_table(META).map_err(redb::Error::from)?;
            table.insert(key, value).map_err(redb::Error::from)?;
        }
        txn.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    fn read_schema_version(&self) -> Result<Option<u32>, PersistError> {
        match self.read_meta(META_SCHEMA_VERSION)? {
            None => Ok(None),
            Some(raw) => raw
                .parse::<u32>()
                .map(Some)
                .map_err(|_| PersistError::InvalidMeta(format!("schema_version = {raw:?}"))),
        }
    }

    fn write_fresh_meta(&self, hostname: &str) -> Result<(), PersistError> {
        self.write_meta(META_SCHEMA_VERSION, &SCHEMA_VERSION.to_string())?;
        self.write_meta(META_HOSTNAME, hostname)?;
        self.write_meta(META_CREATED_US, &now_us().to_string())?;
        Ok(())
    }

    fn check_hostname(&self, expected: &str) -> Result<(), PersistError> {
        match self.read_meta(META_HOSTNAME)? {
            Some(stored) if stored != expected => {
                tracing::warn!(
                    gespeichert = %stored,
                    erwartet = %expected,
                    "Baseline-Datei stammt von einem anderen Host; sie wird weiterverwendet, \
                     die gelernten Normalwerte passen aber möglicherweise nicht"
                );
            }
            Some(_) => {}
            None => self.write_meta(META_HOSTNAME, expected)?,
        }
        Ok(())
    }

    /// Migrationsgerüst: führt alle Schritte von `from` bis
    /// [`SCHEMA_VERSION`] der Reihe nach aus, jeden in eigener Transaktion.
    /// Vor dem ersten Schritt wird die Datei gesichert.
    fn migrate_from(&mut self, from: u32) -> Result<(), PersistError> {
        let backup = self.path.with_extension(format!("redb.bak-{from}"));
        std::fs::copy(&self.path, &backup)?;
        tracing::info!(
            von = from,
            nach = SCHEMA_VERSION,
            sicherung = %backup.display(),
            "migriere Baseline-Datei"
        );

        let mut version = from;
        while version < SCHEMA_VERSION {
            version = self.migrate_one(version)?;
            self.write_meta(META_SCHEMA_VERSION, &version.to_string())?;
        }
        Ok(())
    }

    /// Ein einzelner Migrationsschritt `version -> version + 1`.
    ///
    /// Für Schema v1 gibt es keinen Vorgänger, daher existiert noch kein
    /// Migrationspfad. Sobald v2 eingeführt wird, wird hier ein `match
    /// version { 1 => migrate_v1_to_v2(self), … }` ergänzt; jeder Arm liefert
    /// die neue Versionsnummer zurück.
    fn migrate_one(&mut self, version: u32) -> Result<u32, PersistError> {
        Err(PersistError::NoMigrationPath(version))
    }

    /// Lädt den gesamten Bestand. `Ok(None)`, wenn noch nichts gesichert
    /// wurde (frische Datei).
    pub fn load(&self) -> Result<Option<PersistedState>, PersistError> {
        let txn = self.db.begin_read().map_err(redb::Error::from)?;

        let mut state = PersistedState::default();
        let mut any = false;

        let templates = txn.open_table(TEMPLATES).map_err(redb::Error::from)?;
        for entry in templates.iter().map_err(redb::Error::from)? {
            let (_, value) = entry.map_err(redb::Error::from)?;
            state.templates.clusters.push(serde_json::from_str(value.value())?);
            any = true;
        }

        let baselines = txn.open_table(BASELINES).map_err(redb::Error::from)?;
        for entry in baselines.iter().map_err(redb::Error::from)? {
            let (key, value) = entry.map_err(redb::Error::from)?;
            let hist: CountHistogram = serde_json::from_str(value.value())?;
            state.baselines.histograms.push((unpack_key(key.value()), hist));
            any = true;
        }

        let profiles = txn.open_table(UNIT_PROFILES).map_err(redb::Error::from)?;
        for entry in profiles.iter().map_err(redb::Error::from)? {
            let (key, value) = entry.map_err(redb::Error::from)?;
            let profile: UnitProfile = serde_json::from_str(value.value())?;
            state.baselines.profiles.push((key.value(), profile));
            any = true;
        }

        Ok(any.then_some(state))
    }

    /// Schreibt den gesamten Bestand in **einer** Transaktion. Bei Absturz
    /// mitten im Schreiben bleibt der vorherige Stand gültig (`redb` ist
    /// Copy-on-Write). Vorhandene Einträge werden vollständig ersetzt, damit
    /// von `enforce_limits` verworfene Baselines auch aus der Datei
    /// verschwinden.
    pub fn save(&self, state: &PersistedState) -> Result<(), PersistError> {
        let txn = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut templates = txn.open_table(TEMPLATES).map_err(redb::Error::from)?;
            templates
                .retain(|_, _| false)
                .map_err(redb::Error::from)?;
            for record in &state.templates.clusters {
                let json = serde_json::to_string(record)?;
                templates
                    .insert(record.id.0, json.as_str())
                    .map_err(redb::Error::from)?;
            }

            let mut baselines = txn.open_table(BASELINES).map_err(redb::Error::from)?;
            baselines
                .retain(|_, _| false)
                .map_err(redb::Error::from)?;
            for (key, hist) in &state.baselines.histograms {
                let json = serde_json::to_string(hist)?;
                baselines
                    .insert(pack_key(key), json.as_str())
                    .map_err(redb::Error::from)?;
            }

            let mut profiles = txn.open_table(UNIT_PROFILES).map_err(redb::Error::from)?;
            profiles
                .retain(|_, _| false)
                .map_err(redb::Error::from)?;
            for (unit_key, profile) in &state.baselines.profiles {
                let json = serde_json::to_string(profile)?;
                profiles
                    .insert(*unit_key, json.as_str())
                    .map_err(redb::Error::from)?;
            }

            let mut meta = txn.open_table(META).map_err(redb::Error::from)?;
            meta.insert(META_LAST_SNAPSHOT_US, now_us().to_string().as_str())
                .map_err(redb::Error::from)?;
        }
        txn.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Gesamter Bestand als JSON (`--dump-baselines`). Ersetzt die fehlende
    /// Kommandozeilen-Inspektion einer Key/Value-Datei.
    pub fn dump_json(&self) -> Result<String, PersistError> {
        let state = self.load()?.unwrap_or_default();
        let dump = Dump {
            schema_version: self.read_schema_version()?.unwrap_or(SCHEMA_VERSION),
            hostname: self.read_meta(META_HOSTNAME)?,
            created_us: self.read_meta(META_CREATED_US)?.and_then(|s| s.parse().ok()),
            last_snapshot_us: self
                .read_meta(META_LAST_SNAPSHOT_US)?
                .and_then(|s| s.parse().ok()),
            state: &state,
        };
        Ok(serde_json::to_string_pretty(&dump)?)
    }

    /// Pfad der geöffneten Datei.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Verschiebt eine Datei nach `<pfad>.<grund>-<zeitstempel>` und liefert
/// den neuen Pfad. Für `--reset-baselines` und unlesbare Dateien.
pub fn move_aside(path: &Path, reason: &str) -> Result<PathBuf, std::io::Error> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "baselines.redb".to_string());
    let target = path.with_file_name(format!("{file_name}.{reason}-{}", now_us() / 1_000_000));
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Schlüssel der `baselines`-Tabelle: `unit_key(8, BE) ‖ template_id(8, BE) ‖ slot(1)`.
/// Big-Endian, damit ein Bereichsscan über eine Unit möglich bleibt.
fn pack_key(key: &BaselineKey) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[..8].copy_from_slice(&key.unit_key.to_be_bytes());
    out[8..16].copy_from_slice(&key.template_id.0.to_be_bytes());
    out[16] = key.slot.0;
    out
}

fn unpack_key(raw: [u8; 17]) -> BaselineKey {
    let mut unit = [0u8; 8];
    let mut template = [0u8; 8];
    unit.copy_from_slice(&raw[..8]);
    template.copy_from_slice(&raw[8..16]);
    BaselineKey {
        unit_key: u64::from_be_bytes(unit),
        template_id: TemplateId(u64::from_be_bytes(template)),
        slot: Slot(raw[16]),
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::store::BaselineStore;
    use crate::config::BaselineConfig;
    use crate::template::TemplateEngine;

    const BASE_US: u64 = 1_768_478_400_000_000;

    fn befuellter_zustand() -> PersistedState {
        let cfg = BaselineConfig {
            min_weight: 2.0,
            profile_min_weight: 2.0,
            ..BaselineConfig::default()
        };
        let mut store = BaselineStore::new(5, &cfg);
        for bucket in 0..5u64 {
            store.record(BASE_US + bucket * 2 * 5_000_000, 1, TemplateId(7));
            store.record(BASE_US + bucket * 2 * 5_000_000, 1, TemplateId(7));
            store.record(BASE_US + (bucket * 2 + 1) * 5_000_000, 1, TemplateId(99));
        }
        let mut templates = TemplateEngine::new(0.7, 100);
        templates.process("Started backup job for user alice", BASE_US);
        templates.process("Started backup job for user bob", BASE_US + 1);
        PersistedState {
            baselines: store.snapshot(),
            templates: templates.snapshot(),
        }
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let key = BaselineKey {
            unit_key: 0x0123_4567_89ab_cdef,
            template_id: TemplateId(u64::MAX - 5),
            slot: Slot(47),
        };
        assert_eq!(unpack_key(pack_key(&key)), key);
    }

    #[test]
    fn frische_datei_hat_schema_version_und_keinen_bestand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        let db = BaselineDb::open(&path, "testhost").expect("öffnen");
        assert_eq!(db.read_schema_version().expect("meta"), Some(SCHEMA_VERSION));
        assert_eq!(db.read_meta(META_HOSTNAME).expect("meta").as_deref(), Some("testhost"));
        assert!(db.load().expect("laden").is_none());
    }

    #[test]
    fn save_load_roundtrip_ueber_neu_geoeffnete_datei() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        let zustand = befuellter_zustand();
        assert!(!zustand.baselines.histograms.is_empty());
        assert!(!zustand.templates.clusters.is_empty());

        {
            let db = BaselineDb::open(&path, "testhost").expect("öffnen");
            db.save(&zustand).expect("sichern");
        }

        let db = BaselineDb::open(&path, "testhost").expect("erneut öffnen");
        let geladen = db.load().expect("laden").expect("Bestand vorhanden");

        // Reihenfolge der Histogramme ist über redb-Schlüsselordnung
        // definiert, nicht über die HashMap-Reihenfolge des Snapshots:
        // deshalb mengenweise vergleichen.
        let mut a = zustand.baselines.histograms.clone();
        let mut b = geladen.baselines.histograms.clone();
        a.sort_by_key(|(k, _)| pack_key(k));
        b.sort_by_key(|(k, _)| pack_key(k));
        assert_eq!(a, b);

        let mut pa = zustand.baselines.profiles.clone();
        let mut pb = geladen.baselines.profiles.clone();
        pa.sort_by_key(|(k, _)| *k);
        pb.sort_by_key(|(k, _)| *k);
        assert_eq!(pa, pb);

        assert_eq!(geladen.templates, zustand.templates);
    }

    #[test]
    fn save_ersetzt_alten_bestand_vollstaendig() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        let db = BaselineDb::open(&path, "testhost").expect("öffnen");
        db.save(&befuellter_zustand()).expect("sichern");

        // Zweiter Snapshot ist leer -> die Datei darf nichts mehr enthalten,
        // sonst kämen von enforce_limits verworfene Baselines zurück.
        db.save(&PersistedState::default()).expect("leer sichern");
        assert!(db.load().expect("laden").is_none());
    }

    #[test]
    fn neuere_schema_version_wird_verweigert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        {
            let db = BaselineDb::open(&path, "testhost").expect("öffnen");
            db.write_meta(META_SCHEMA_VERSION, &(SCHEMA_VERSION + 1).to_string())
                .expect("meta");
        }
        let err = BaselineDb::open(&path, "testhost").expect_err("muss verweigern");
        assert!(
            matches!(err, PersistError::NewerSchema { found, supported }
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
            "war {err:?}"
        );
    }

    #[test]
    fn unbekannte_aeltere_version_liefert_klaren_fehler_und_sicherung() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        {
            let db = BaselineDb::open(&path, "testhost").expect("öffnen");
            db.write_meta(META_SCHEMA_VERSION, "0").expect("meta");
        }
        let err = BaselineDb::open(&path, "testhost").expect_err("kein Migrationspfad");
        assert!(matches!(err, PersistError::NoMigrationPath(0)), "war {err:?}");
        assert!(
            dir.path().join("baselines.redb.bak-0").exists(),
            "vor der Migration muss eine Sicherung existieren"
        );
    }

    #[test]
    fn unlesbare_datei_wird_verschoben_statt_den_start_zu_verhindern() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        std::fs::write(&path, b"das ist keine redb-datei, nur muell").expect("schreiben");

        let db = BaselineDb::open(&path, "testhost").expect("muss trotzdem öffnen");
        assert!(db.load().expect("laden").is_none());

        let verschoben: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("baselines.redb.corrupt-"))
            .collect();
        assert_eq!(verschoben.len(), 1, "genau eine verschobene Datei erwartet: {verschoben:?}");
    }

    #[test]
    fn abweichender_hostname_wird_toleriert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        {
            let db = BaselineDb::open(&path, "host-a").expect("öffnen");
            db.save(&befuellter_zustand()).expect("sichern");
        }
        // Anderer Host: warnt, lernt aber weiter -- der Bestand bleibt lesbar.
        let db = BaselineDb::open(&path, "host-b").expect("öffnen");
        assert!(db.load().expect("laden").is_some());
    }

    #[test]
    fn dump_json_ist_gueltiges_json_mit_meta_und_bestand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        let db = BaselineDb::open(&path, "testhost").expect("öffnen");
        db.save(&befuellter_zustand()).expect("sichern");

        let dump = db.dump_json().expect("dump");
        let parsed: serde_json::Value = serde_json::from_str(&dump).expect("gültiges JSON");
        assert_eq!(parsed["schema_version"], SCHEMA_VERSION);
        assert_eq!(parsed["hostname"], "testhost");
        assert!(parsed["state"]["baselines"]["histograms"]
            .as_array()
            .is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn move_aside_benennt_um_und_liefert_neuen_pfad() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("baselines.redb");
        std::fs::write(&path, b"x").expect("schreiben");
        let ziel = move_aside(&path, "bak-manual").expect("verschieben");
        assert!(!path.exists());
        assert!(ziel.exists());
        assert!(ziel
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with("baselines.redb.bak-manual-"));
    }
}
