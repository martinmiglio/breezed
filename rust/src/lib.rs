//! breezed — curve-based fan controller for Dell `PowerEdge` servers via iDRAC/IPMI.
//!
//! The crate mirrors the original Python package's onion architecture:
//! `domain` ← `application` ← `adapters` ← `entry`. Dependencies only point
//! inward; `entry` is the sole composition root.

#![warn(missing_docs)]

pub mod adapters;
pub mod application;
pub mod domain;
pub mod entry;

/// Version reported by `breezed --version` and `daemon status`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
