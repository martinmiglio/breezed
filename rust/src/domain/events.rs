//! Typed control-loop events: the single, closed log vocabulary.

use crate::domain::types::{EventType, FanPercent, OperatingMode, Operation, Reason, TempC};

/// One observable control-loop event.
///
/// Variants own their payloads and encode field order, so every sink renders
/// the same JSON regardless of how it consumes the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The control loop started.
    Startup,
    /// A temperature poll completed.
    Poll {
        /// Maximum CPU temperature read this tick.
        temp_c: TempC,
        /// Last successfully commanded manual duty, if any.
        fan_pct: Option<FanPercent>,
        /// Operating mode after the decision.
        mode: OperatingMode,
        /// Raw policy target; `None` means AUTO.
        target_pct: Option<i32>,
    },
    /// The operating mode changed.
    ModeChange {
        /// Mode before the change.
        from: OperatingMode,
        /// Mode after the change.
        to: OperatingMode,
        /// Why the change happened.
        reason: Reason,
        /// Temperature that triggered the change, when applicable.
        temp_c: Option<TempC>,
        /// Consecutive read failures that triggered the change, when applicable.
        failures: Option<u32>,
    },
    /// A manual fan speed was applied.
    SpeedChange {
        /// Duty cycle actually applied.
        fan_pct: FanPercent,
        /// Policy target that motivated the write.
        target_pct: i32,
        /// Why the speed changed.
        reason: Reason,
    },
    /// A downward step is waiting out the hysteresis window.
    HysteresisWait {
        /// Target the controller is waiting to apply.
        target_pct: i32,
        /// Configured hysteresis window in seconds.
        hysteresis_s: i64,
    },
    /// A hot-reloaded config was accepted.
    ConfigReload,
    /// A config read or validation failed.
    ConfigError {
        /// Field-naming failure message.
        error: String,
    },
    /// An IPMI read or write failed.
    IpmiError {
        /// Write operation that failed; absent for read failures.
        operation: Option<Operation>,
        /// Decision that motivated the write; absent for read failures.
        reason: Option<Reason>,
        /// Consecutive failure count.
        failures: u32,
        /// Transport error text (already redacted).
        error: String,
    },
    /// The control loop is stopping.
    Shutdown,
}

impl Event {
    /// The closed vocabulary member this event belongs to.
    pub fn kind(&self) -> EventType {
        match self {
            Self::Startup => EventType::Startup,
            Self::Poll { .. } => EventType::Poll,
            Self::ModeChange { .. } => EventType::ModeChange,
            Self::SpeedChange { .. } => EventType::SpeedChange,
            Self::HysteresisWait { .. } => EventType::HysteresisWait,
            Self::ConfigReload => EventType::ConfigReload,
            Self::ConfigError { .. } => EventType::ConfigError,
            Self::IpmiError { .. } => EventType::IpmiError,
            Self::Shutdown => EventType::Shutdown,
        }
    }

    /// Log level used for this event. Every control-loop event is INFO today.
    pub const fn level(&self) -> &'static str {
        "INFO"
    }
}
