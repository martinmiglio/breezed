//! Concrete I/O adapters: ipmitool, TOML+env config, mtime watcher, JSON logs,
//! and the Prometheus socket.

pub mod config;
pub mod ipmi;
#[cfg(feature = "native-ipmi")]
pub mod ipmi_native;
pub mod logs;
pub mod metrics;
pub mod watcher;
