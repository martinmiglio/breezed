//! Capability traits and the speed-policy / clock seams.

use crate::domain::curve::CurvePoint;
use crate::domain::events::Event;
use crate::domain::types::{DomainError, FanPercent, IpmiError, TempC};

/// Reads the hottest CPU temperature from the server.
pub trait TempReader: Send + Sync {
    /// Read the maximum CPU temperature.
    ///
    /// # Errors
    ///
    /// Returns [`IpmiError`] on transport or parse failures.
    fn read_max_cpu_temp(&self) -> Result<TempC, IpmiError>;
}

/// Commands the server's fan controller.
pub trait FanCommander: Send + Sync {
    /// Hand control back to the iDRAC.
    ///
    /// # Errors
    ///
    /// Returns [`IpmiError`] on transport failures.
    fn enable_auto(&self) -> Result<(), IpmiError>;

    /// Take control away from the iDRAC.
    ///
    /// # Errors
    ///
    /// Returns [`IpmiError`] on transport failures.
    fn disable_auto(&self) -> Result<(), IpmiError>;

    /// Apply a fixed manual duty cycle.
    ///
    /// # Errors
    ///
    /// Returns [`IpmiError`] on transport failures.
    fn set_manual_pct(&self, pct: FanPercent) -> Result<(), IpmiError>;
}

/// A device that both reads temperatures and commands fans.
pub trait FanDevice: TempReader + FanCommander {}

impl<T: TempReader + FanCommander> FanDevice for T {}

/// Receives every event the controller emits.
pub trait EventSink: Send + Sync {
    /// Handle one event.
    fn emit(&self, event: &Event);
}

/// Maps a curve and a temperature to a fan target (`None` means AUTO).
pub trait SpeedPolicy: Send + Sync {
    /// Compute the target for `temp_c`, or `None` to hand control to iDRAC.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] for an invalid curve.
    fn target(&self, curve: &[CurvePoint], temp_c: TempC) -> Result<Option<i32>, DomainError>;
}

/// Monotonic seconds, injectable for deterministic hysteresis tests.
pub trait Clock: Send + Sync {
    /// Seconds from an arbitrary monotonic origin.
    fn monotonic(&self) -> f64;
}

/// Production clock backed by [`std::time::Instant`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        use std::sync::OnceLock;
        use std::time::Instant;

        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        let origin = *ORIGIN.get_or_init(Instant::now);
        origin.elapsed().as_secs_f64()
    }
}
