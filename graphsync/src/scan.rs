//! Rekursives Durchsuchen der Quellverzeichnisse. Liefert nur Metadaten
//! (Pfad, Größe, mtime) -- gelesen und gehasht wird nur, was sich laut
//! Metadaten geändert hat (siehe [`crate::sync`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Metadaten einer gefundenen Datei.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Pfad wie gefunden (Wurzel + relativer Teil).
    pub path: PathBuf,
    /// Index der Quellwurzel, unter der die Datei gefunden wurde.
    pub root: usize,
    pub size: u64,
    /// Änderungszeit in Nanosekunden seit der Unix-Epoche (0 = unbekannt).
    pub mtime_ns: i64,
}

/// Ergebnis eines Durchlaufs.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Gefundene Dateien, nach Pfad sortiert.
    pub files: Vec<FileMeta>,
    /// `true`, wenn `max_files` erreicht wurde und Dateien fehlen.
    pub truncated: bool,
    /// Verzeichnisse, die nicht gelesen werden konnten.
    pub unreadable_dirs: usize,
}

/// Parameter eines Durchlaufs.
pub struct ScanOptions<'a> {
    pub extensions: &'a [String],
    pub exclude_dir_names: &'a [String],
    pub skip_hidden_dirs: bool,
    pub max_files: usize,
}

/// Durchsucht alle `roots` rekursiv. Symbolische Links werden nicht
/// verfolgt (keine Zyklen, kein Verlassen der Wurzeln); überlappende
/// Wurzeln liefern jede Datei nur einmal.
pub fn scan(roots: &[PathBuf], opts: &ScanOptions<'_>) -> ScanResult {
    let mut result = ScanResult::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    'roots: for (root_idx, root) in roots.iter().enumerate() {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                result.unreadable_dirs += 1;
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if file_type.is_dir() {
                    if !excluded_dir(&path, opts) {
                        stack.push(path);
                    }
                    continue;
                }
                if !file_type.is_file() || !has_extension(&path, opts.extensions) {
                    continue;
                }
                if !seen.insert(path.clone()) {
                    continue;
                }
                if result.files.len() >= opts.max_files {
                    result.truncated = true;
                    break 'roots;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
                result.files.push(FileMeta {
                    path,
                    root: root_idx,
                    size: meta.len(),
                    mtime_ns,
                });
            }
        }
    }
    result.files.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

fn excluded_dir(path: &Path, opts: &ScanOptions<'_>) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    (opts.skip_hidden_dirs && name.starts_with('.'))
        || opts.exclude_dir_names.iter().any(|ex| ex == name)
}

fn has_extension(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| extensions.iter().any(|want| want.eq_ignore_ascii_case(ext)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts<'a>(ext: &'a [String], ex: &'a [String], max: usize) -> ScanOptions<'a> {
        ScanOptions {
            extensions: ext,
            exclude_dir_names: ex,
            skip_hidden_dirs: true,
            max_files: max,
        }
    }

    #[test]
    fn findet_markdown_ueberspringt_ausgeschlossenes_und_verstecktes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for p in [
            "a.md",
            "b.MD",
            "c.txt",
            "sub/d.md",
            "node_modules/e.md",
            ".hidden/f.md",
        ] {
            let full = root.join(p);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, "x").expect("write");
        }
        let ext = vec!["md".to_string()];
        let ex = vec!["node_modules".to_string()];
        let result = scan(&[root.to_path_buf()], &opts(&ext, &ex, 100));
        let names: Vec<String> = result
            .files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(root)
                    .expect("prefix")
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["a.md", "b.MD", "sub/d.md"]);
        assert!(!result.truncated);
    }

    #[test]
    fn obergrenze_wird_eingehalten_und_gemeldet() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.md")), "x").expect("write");
        }
        let ext = vec!["md".to_string()];
        let result = scan(&[dir.path().to_path_buf()], &opts(&ext, &[], 3));
        assert_eq!(result.files.len(), 3);
        assert!(result.truncated);
    }

    #[test]
    fn fehlende_wurzel_wird_gezaehlt_nicht_abgebrochen() {
        let ext = vec!["md".to_string()];
        let result = scan(&[PathBuf::from("/gibt/es/nicht")], &opts(&ext, &[], 3));
        assert!(result.files.is_empty());
        assert_eq!(result.unreadable_dirs, 1);
    }
}
