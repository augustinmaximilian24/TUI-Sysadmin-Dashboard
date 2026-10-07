//! `logsentry-graphsync` (Phase 14, optional): hält die Verknüpfungen
//! zwischen Markdown-Dateien automatisch aktuell, ohne dass dafür jemand
//! manuell eine KI anstoßen muss.
//!
//! Ablauf je Takt (siehe [`sync::Syncer`]):
//! 1. [`scan`]: Quellverzeichnisse durchsuchen (nur Metadaten).
//! 2. Delta: nur Dateien mit geänderter Größe/mtime werden gelesen und
//!    gehasht; nur bei geändertem Hash neu extrahiert ([`extract`]).
//! 3. [`graph`]: Ausgabe im `graph.json`-Format schreiben (nur bei
//!    Änderung) -- die GUI mischt sie als Overlay in beide Graph-Ansichten.
//! 4. [`llm`]: optional und hart begrenzt geänderte Dateien an
//!    `claude -p` geben; Ergebnis landet im Zustand ([`state`]) und fließt
//!    beim nächsten Schreiben als `INFERRED`-Kanten ein.
//!
//! Läuft als Benutzerprozess (systemd-User-Unit), nicht im Daemon.

pub mod extract;
pub mod graph;
pub mod llm;
pub mod scan;
pub mod state;
pub mod sync;
