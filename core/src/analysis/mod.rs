//! Statistische Anomalie-Erkennung ohne ML-Modelle.
//!
//! Aufbau:
//! - [`stats`]: zustandsfreie Mathematik (Entropie, Median/MAD, Z-Score, Surprisal)
//! - [`window`]: gleitendes Zeitfenster über die Template-Verteilung
//! - [`rate`]: bucketierte Rate-Historie pro Template
//! - [`engine`]: Kombination der Signale, Hysterese, Cooldown, Lernphase

pub mod engine;
pub mod rate;
pub mod stats;
pub mod window;

pub use engine::{
    explain_breakdown, AnalysisEngine, AnalysisInput, AnalysisStats, Anomaly, AnomalyLevel,
    ScoreBreakdown,
};
pub use rate::RateTracker;
pub use window::SlidingWindow;
