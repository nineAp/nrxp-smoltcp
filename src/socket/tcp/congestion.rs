use crate::{socket::tcp::RateSample, time::Instant};

use super::RttEstimator;

pub(super) mod no_control;

#[cfg(feature = "socket-tcp-cubic")]
pub(super) mod cubic;

#[cfg(feature = "socket-tcp-reno")]
pub(super) mod reno;

#[cfg(feature = "socket-tcp-bbr")]
pub(super) mod bbr;

#[allow(unused_variables)]
pub(super) trait Controller {
    /// Returns the number of bytes that can be sent.
    fn window(&self) -> usize;

    /// Set the remote window size.
    fn set_remote_window(&mut self, remote_window: usize) {}

    fn on_ack(&mut self, now: Instant, len: usize, rtt: &RttEstimator) {}

    fn on_ack_with_rate(
        &mut self,
        now: Instant,
        acked_len: usize,
        rtte: &RttEstimator,
        _sample: RateSample,
    ) {
        // By default, fall back to the plain ACK path.
        self.on_ack(now, acked_len, rtte);
    }
    fn pacing_rate(&self) -> u64 {
        0
    }

    fn get_estimated_bdp(&self) -> usize {
        0
    }
    fn on_retransmit(&mut self, now: Instant) {}

    fn on_duplicate_ack(&mut self, now: Instant) {}

    fn pre_transmit(&mut self, now: Instant) {}

    fn post_transmit(&mut self, now: Instant, len: usize) {}

    /// Set the maximum segment size.
    fn set_mss(&mut self, mss: usize) {}
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(super) enum AnyController {
    None(no_control::NoControl),

    #[cfg(feature = "socket-tcp-reno")]
    Reno(reno::Reno),

    #[cfg(feature = "socket-tcp-cubic")]
    Cubic(cubic::Cubic),

    #[cfg(feature = "socket-tcp-bbr")]
    Bbr(bbr::Bbr),
}

impl AnyController {
    /// Create a new congestion controller.
    /// `AnyController::new()` selects the best congestion controller based on the features.
    ///
    /// - If `socket-tcp-cubic` feature is enabled, it will use `Cubic`.
    /// - If `socket-tcp-reno` feature is enabled, it will use `Reno`.
    /// - If both `socket-tcp-cubic` and `socket-tcp-reno` features are enabled, it will use `Cubic`.
    ///    - `Cubic` is more efficient regarding throughput.
    ///    - `Reno` is more conservative and is suitable for low-power devices.
    /// - If no congestion controller is available, it will use `NoControl`.
    ///
    /// Users can also select a congestion controller manually by [`super::Socket::set_congestion_control()`]
    /// method at run-time.
    #[allow(unreachable_code)]
    #[inline]
    pub fn new() -> Self {
        #[cfg(feature = "socket-tcp-cubic")]
        {
            return AnyController::Cubic(cubic::Cubic::new());
        }

        #[cfg(feature = "socket-tcp-reno")]
        {
            return AnyController::Reno(reno::Reno::new());
        }

        AnyController::None(no_control::NoControl)
    }

    #[inline]
    pub fn inner_mut(&mut self) -> &mut dyn Controller {
        match self {
            AnyController::None(n) => n,

            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r,

            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c,

            #[cfg(feature = "socket-tcp-bbr")]
            AnyController::Bbr(c) => c,
        }
    }

    #[inline]
    pub fn inner(&self) -> &dyn Controller {
        match self {
            AnyController::None(n) => n,

            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r,

            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c,

            #[cfg(feature = "socket-tcp-bbr")]
            AnyController::Bbr(c) => c,
        }
    }

    #[allow(dead_code)]
    fn pacing_rate(&self) -> u64 {
        match self {
            AnyController::None(c) => c.pacing_rate(),
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(c) => c.pacing_rate(),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.pacing_rate(),
            #[cfg(feature = "socket-tcp-bbr")]
            AnyController::Bbr(c) => c.pacing_rate(),
        }
    }

    #[allow(dead_code)]
    fn on_ack_with_rate(
        &mut self,
        now: Instant,
        acked_len: usize,
        rtte: &RttEstimator,
        sample: RateSample,
    ) {
        match self {
            AnyController::None(c) => c.on_ack_with_rate(now, acked_len, rtte, sample),
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(c) => c.on_ack_with_rate(now, acked_len, rtte, sample),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_ack_with_rate(now, acked_len, rtte, sample),
            #[cfg(feature = "socket-tcp-bbr")]
            AnyController::Bbr(c) => c.on_ack_with_rate(now, acked_len, rtte, sample),
        }
    }
}
