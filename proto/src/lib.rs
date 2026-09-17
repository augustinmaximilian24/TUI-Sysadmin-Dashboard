//! `logsentry-proto`: Wire-Format zwischen Daemon und Client(s) (GUI,
//! später TUI).
//!
//! Entwurf und normative Entscheidungen: `docs/phase6-protokoll.md`.
//! `proto` hängt bewusst nicht von `logsentry-core` ab — siehe dortiger
//! Abschnitt 1.

#[cfg(feature = "client")]
mod client;
mod error;
mod framing;
mod wire;

#[cfg(feature = "client")]
pub use client::{spawn, ClientConfig, ClientError, ConnectionState};
pub use error::{FrameError, ProtoError};
pub use framing::{read_frame, write_frame, FrameReader};
pub use wire::*;
