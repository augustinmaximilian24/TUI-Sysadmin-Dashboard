//! Baselines pro Unit mit Tageszeit-/Wochentagsprofil (Phase 4).
//!
//! Siehe `docs/phase4-baselines.md` für den vollständigen Entwurf.

pub mod decay;
pub mod histogram;
pub mod slot;

pub use decay::DecayParams;
pub use histogram::{CountHistogram, HistogramConfig};
pub use slot::Slot;
