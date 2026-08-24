//! ViryaOS deterministic Autopilot application boundary.
//!
//! Pure bounded contexts live in `crowdrelay-domain`; this module only exposes
//! typed orchestration, infrastructure ports and the exception-first operator
//! control surface. Splitting concerns here keeps SQL/HTTP/provider details out
//! of business decisions without adding another crate to the compile graph.

mod control;
mod evaluate;
mod growth_posture;
mod model;
mod ports;

pub use control::*;
pub use evaluate::*;
pub use growth_posture::*;
pub use model::*;
pub use ports::*;
