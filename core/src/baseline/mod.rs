//! Baselines pro Unit mit Tageszeit-/Wochentagsprofil (Phase 4).
//!
//! Siehe `docs/phase4-baselines.md` für den vollständigen Entwurf.

pub mod decay;
pub mod histogram;
pub mod profile;
pub mod slot;
pub mod store;

pub use decay::DecayParams;
pub use histogram::{CountHistogram, HistogramConfig};
pub use profile::{ProfileConfig, UnitProfile};
pub use slot::Slot;
pub use store::{BaselineKey, BaselineSnapshot, BaselineStore, RateSource};
