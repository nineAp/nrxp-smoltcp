use crate::socket::tcp::RateSample;
use crate::socket::tcp::congestion::Controller;
use crate::time::{Duration, Instant};

// ─── Constants ────────────────────────────────────────────────────────────────
// All gain values are scaled relative to 1024 (= 1.0x):
//   BBR_HIGH_GAIN  = 2885/1024 ≈ 2.885x  (2/ln2 ≈ 2.89, per BBR v1)
//   BBR_DRAIN_GAIN = 1024/2885 ≈ 0.346x  (its reciprocal)
//   BBR_PROBE_GAIN = 1280/1024 = 1.25x   (probe-up in ProbeBw)
//   BBR_PROBE_DRAIN_GAIN = 819/1024 ≈ 0.8x (drain in ProbeBw)
const BBR_UNIT: u64 = 1024;
const BBR_HIGH_GAIN: u64 = 2885;
const BBR_DRAIN_GAIN: u64 = BBR_UNIT * BBR_UNIT / BBR_HIGH_GAIN; // ≈ 363
const BBR_PROBE_GAIN: u64 = 1280;
const BBR_PROBE_DRAIN_GAIN: u64 = BBR_UNIT * BBR_UNIT / BBR_PROBE_GAIN; // ≈ 819

// Width of the RTprop min-filter window, in rounds. A window of 1 would mean no
// filtering at all — the last sample would always win.
const RTPROP_FILTER_ROUNDS: usize = 10;
// After this long without a fresh RTprop minimum, the estimate is considered
// stale and ProbeRtt is entered.
const RTPROP_FILTER_LEN: Duration = Duration::from_secs(10);

const BTLBW_FILTER_LEN: usize = 10;
const MSS: usize = 1460;

// Duration of the ProbeRtt phase per BBR v1 §4.3.3. Fixed at 200 ms — it is not
// derived from the measured RTT.
const PROBE_RTT_DURATION: Duration = Duration::from_millis(200);

// Startup exit threshold: bandwidth growth below 25% over three rounds.
// curr * 4 < prev * 5  ⟺  curr < 1.25 * prev
const STARTUP_GROWTH_THRESHOLD_NUM: u64 = 4;
const STARTUP_GROWTH_THRESHOLD_DEN: u64 = 5;

// Consecutive rounds without growth required to leave Startup.
const STARTUP_FULL_BW_COUNT_THRESHOLD: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
enum BbrState {
    Startup,
    Drain,
    ProbeBw,
    ProbeRtt,
}

// ─── Minmax ────────────────────────────────────────────────────────────────────
// Sliding-window min/max filter (the Kathleen Nichols / Van Jacobson algorithm).
// `window` is the window size in units of `time` (here: a round number).
// It keeps three samples so that the extremum over the window stays cheap to find.
//
// Note: for the min filter the initial value T::default() (0 for u64) is the
// smallest one representable, so a real measurement would never satisfy
// `meas <= values[0].1`. The `initialized` flag solves that: the first
// measurement is always accepted regardless of its value.
#[derive(Debug, Clone, Copy)]
struct Minmax<T> {
    window: usize,
    values: [(usize, T); 3],
    initialized: bool,
}

impl<T: Copy + PartialOrd + Default> Minmax<T> {
    fn new(window: usize) -> Self {
        Self {
            window,
            values: [(0, T::default()); 3],
            initialized: false,
        }
    }

    fn update_max(&mut self, time: usize, meas: T) -> T {
        if !self.initialized
            || time >= self.values[0].0 + self.window
            || meas >= self.values[0].1
        {
            self.initialized = true;
            self.values[0] = (time, meas);
            self.values[1] = (time, meas);
            self.values[2] = (time, meas);
        } else {
            if meas >= self.values[1].1 {
                self.values[1] = (time, meas);
                self.values[2] = (time, meas);
            } else if meas >= self.values[2].1 {
                self.values[2] = (time, meas);
            }
            if time >= self.values[1].0 + self.window {
                self.values[1] = self.values[2];
                self.values[2] = (time, meas);
            } else if time >= self.values[2].0 + self.window {
                self.values[2] = (time, meas);
            }
        }
        self.values[0].1
    }

    fn update_min(&mut self, time: usize, meas: T) -> T {
        if !self.initialized
            || time >= self.values[0].0 + self.window
            || meas <= self.values[0].1
        {
            self.initialized = true;
            self.values[0] = (time, meas);
            self.values[1] = (time, meas);
            self.values[2] = (time, meas);
        } else {
            if meas <= self.values[1].1 {
                self.values[1] = (time, meas);
                self.values[2] = (time, meas);
            } else if meas <= self.values[2].1 {
                self.values[2] = (time, meas);
            }
            if time >= self.values[1].0 + self.window {
                self.values[1] = self.values[2];
                self.values[2] = (time, meas);
            } else if time >= self.values[2].0 + self.window {
                self.values[2] = (time, meas);
            }
        }
        self.values[0].1
    }

    fn get(&self) -> T {
        self.values[0].1
    }
}

// ─── BBR struct ────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy)]
pub struct Bbr {
    state: BbrState,

    /// Sliding-window maximum of the delivery rate (bottleneck bandwidth).
    btlbw: Minmax<u64>,

    /// Sliding-window minimum RTT (round-trip propagation delay), filtered over
    /// RTPROP_FILTER_ROUNDS rounds.
    rtprop: Minmax<u64>,

    /// When the RTprop minimum was last refreshed.
    rtprop_stamp: Instant,

    // ── Round tracking ─────────────────────────────────────────────────────
    /// Index of the current delivery round, incremented once per RTT.
    round_count: usize,
    /// Value of `delivered` at which the next round begins. It is taken from the
    /// delivery count recorded when the acknowledged packet was sent, so the
    /// round is complete once `self.delivered >= next_round_delivered`.
    next_round_delivered: u64,

    // ── Delivery counters (kept in sync with tcp.rs through RateSample) ────
    /// Total bytes acknowledged over the lifetime of the connection.
    delivered: u64,
    /// When `delivered` was last updated.
    delivered_time: Instant,

    // ── Data in flight ─────────────────────────────────────────────────────
    inflight: usize,

    // ── Window and send pace ───────────────────────────────────────────────
    cwnd: usize,
    pacing_rate: u64,
    pacing_gain: u64,
    cwnd_gain: u64,

    // ── ProbeRtt state ─────────────────────────────────────────────────────
    probe_rtt_done_stamp: Option<Instant>,

    // ── ProbeBw state ──────────────────────────────────────────────────────
    /// Phase index 0..8 in the ProbeBw cycle (0 = probe-up, 1 = drain, 2-7 = cruise).
    cycle_index: usize,
    /// Start of the current ProbeBw phase, used to hold probe-up to one round.
    cycle_stamp: Instant,

    // ── Startup ────────────────────────────────────────────────────────────
    /// Best delivery rate seen during Startup (`full_bw` in the Linux BBR).
    startup_full_bw: u64,
    /// Consecutive rounds in which the delivery rate failed to grow by 25%,
    /// compared against STARTUP_FULL_BW_COUNT_THRESHOLD.
    startup_full_bw_count: u8,

    // ── Current MSS, updated through set_mss ──────────────────────────────
    mss: usize,

    // ── Last round in which the Startup check ran ─────────────────────────
    last_startup_round: usize,

    // ── RTT override for tunnelled paths ──────────────────────────────────
    tunnel_rtt_override: Option<Duration>,
}

impl Bbr {
    pub fn new() -> Self {
        let zero = Instant::from_millis(0);
        Self {
            state: BbrState::Startup,
            btlbw: Minmax::new(BTLBW_FILTER_LEN),
            rtprop: Minmax::new(RTPROP_FILTER_ROUNDS),
            rtprop_stamp: zero,
            round_count: 0,
            next_round_delivered: 0,
            delivered: 0,
            delivered_time: zero,
            inflight: 0,
            mss: MSS,
            cwnd: MSS * 4,
            pacing_rate: 0,
            pacing_gain: BBR_HIGH_GAIN,
            cwnd_gain: BBR_HIGH_GAIN,
            probe_rtt_done_stamp: None,
            cycle_index: 0,
            cycle_stamp: zero,
            startup_full_bw: 0,
            startup_full_bw_count: 0,
            last_startup_round: 0,
            tunnel_rtt_override: None,
        }
    }

    /// Override the RTT used for the BDP estimate, for paths where the RTT that
    /// matters is measured a layer below this socket. Pass `None` to go back to
    /// the RTprop measured here.
    pub fn set_tunnel_rtt_override(&mut self, rtt: Option<Duration>) {
        self.tunnel_rtt_override = rtt;
    }

    // ─── BDP ─────────────────────────────────────────────────────────────────
    // Units: bw [bytes/s] * rtt [µs] / 1_000_000 [µs/s] = BDP [bytes].
    fn get_bdp(&self) -> usize {
        let bw = self.btlbw.get();
        let rtt_micros = if let Some(ovr) = self.tunnel_rtt_override {
            ovr.micros()
        } else {
            self.rtprop.get()
        };
        if bw == 0 || rtt_micros == 0 {
            return self.mss * 4;
        }
        // u128 for the intermediate product: at bw ≈ 10 Gbit/s (1.25e9 bytes/s)
        // and rtt ≈ 1_000_000 µs it reaches ~1.25e15.
        // Saturate instead of truncating: on a 32-bit target (armv7/x86 Android) a
        // plain `as usize` wraps, and a huge BtlBw from a microsecond-interval sample
        // on a local virtual link would then turn into a tiny, garbage BDP.
        let bdp = (bw as u128 * rtt_micros as u128 / 1_000_000).min(usize::MAX as u128) as usize;
        bdp.max(self.mss * 4)
    }

    // ─── Control parameter update ────────────────────────────────────────────
    fn update_control_parameters(&mut self) {
        let max_bw = self.btlbw.get();

        // pacing_gain is scaled relative to BBR_UNIT = 1024, so >> 10 undoes it.
        self.pacing_rate =
            ((self.pacing_gain as u128 * max_bw as u128) >> 10).min(u64::MAX as u128) as u64;

        match self.state {
            BbrState::ProbeRtt => {
                // Minimise inflight to get a clean RTT measurement.
                // BBR v1 §4.3.3: cwnd = 4 * SMSS.
                self.cwnd = self.mss * 4;
            }
            _ => {
                // cwnd_gain is in the same 1024 base, so >> 10 undoes it.
                let target = ((self.cwnd_gain as u128 * self.get_bdp() as u128) >> 10)
                    .min(usize::MAX as u128) as usize;
                self.cwnd = target.max(self.mss * 4);
            }
        }
    }

    // ─── Pipe-full detection during Startup ──────────────────────────────────
    // BBR v1 §4.3.1, following the Linux kernel implementation. The current
    // delivery rate is compared against the best seen so far (startup_full_bw):
    // growth of 25% or more refreshes full_bw and clears the counter, otherwise
    // the counter advances. Three rounds without growth leave Startup.
    // Runs at most once per round, guarded by last_startup_round.
    fn check_startup_full_bandwidth(&mut self, delivery_rate: u64) {
        if self.round_count <= self.last_startup_round {
            return;
        }
        self.last_startup_round = self.round_count;

        // Growth of >= 25%: delivery_rate * 4 >= startup_full_bw * 5.
        // u128 for the products, so u64 cannot overflow.
        let grew = (delivery_rate as u128) * (STARTUP_GROWTH_THRESHOLD_NUM as u128)
            >= (self.startup_full_bw as u128) * (STARTUP_GROWTH_THRESHOLD_DEN as u128);

        if grew || self.startup_full_bw == 0 {
            if delivery_rate > self.startup_full_bw {
                self.startup_full_bw = delivery_rate;
            }
            self.startup_full_bw_count = 0;
        } else {
            self.startup_full_bw_count += 1;
        }
    }

    // ─── Entering ProbeBw ────────────────────────────────────────────────────
    fn enter_probe_bw(&mut self, now: Instant) {
        self.state = BbrState::ProbeBw;
        // Start in cruise; cycle_index = 7 makes the first increment in
        // on_ack_with_rate land on 0, so the next round probes up. Coming out of
        // Drain the excess is already gone, so one cruise round is enough before
        // probing again.
        self.pacing_gain = BBR_UNIT; // cruise
        self.cycle_index = 7;
        self.cycle_stamp = now;
        self.cwnd_gain = BBR_UNIT + BBR_UNIT / 4; // 1.25x cwnd in ProbeBw
    }
}

// ─── Controller impl ──────────────────────────────────────────────────────────
impl Controller for Bbr {
    fn set_mss(&mut self, mss: usize) {
        if mss > 0 {
            self.mss = mss;
            // Raise cwnd to the new floor if it sits below it.
            if self.cwnd < self.mss * 4 {
                self.cwnd = self.mss * 4;
            }
        }
    }

    fn window(&self) -> usize {
        self.cwnd
    }

    fn pacing_rate(&self) -> u64 {
        self.pacing_rate
    }

    fn get_estimated_bdp(&self) -> usize {
        self.get_bdp()
    }

    fn on_ack(
        &mut self,
        _now: Instant,
        acked_len: usize,
        _rtte: &crate::socket::tcp::RttEstimator,
    ) {
        // BBR needs a delivery-rate sample for a full update. Without one, only
        // inflight moves and cwnd is recomputed from the current estimates.
        self.inflight = self.inflight.saturating_sub(acked_len);
        self.update_control_parameters();
    }

    fn on_ack_with_rate(
        &mut self,
        now: Instant,
        acked_len: usize,
        _rtte: &crate::socket::tcp::RttEstimator,
        sample: RateSample,
    ) {
        // ── 1. Update the delivery counters ──────────────────────────────────
        self.inflight = self.inflight.saturating_sub(acked_len);
        self.delivered += acked_len as u64;
        self.delivered_time = now;

        // ── 2. Update BtlBw and RTprop before counting rounds ────────────────
        // Order matters: the BDP used for `advance` below must already be fresh.
        // BtlBw is not updated on app-limited samples, where the delivery rate is
        // understated by the application rather than by the path.
        if !sample.is_app_limited && sample.delivery_rate > 0 {
            self.btlbw
                .update_max(self.round_count, sample.delivery_rate);
        }
        if let Some(rtt) = sample.rtt {
            let rtt_micros = rtt.micros();
            if rtt_micros > 0 {
                let new_min = self.rtprop.update_min(self.round_count, rtt_micros);
                if rtt_micros <= new_min {
                    self.rtprop_stamp = now;
                }
            }
        }

        // ── 3. Advance the round counter ─────────────────────────────────────
        // A BBR round ends when an ACK arrives for a packet sent at the start of
        // that round. prior_delivered — the delivery count recorded when the
        // acknowledged packet was sent — is the natural round boundary.
        if self.next_round_delivered == 0 {
            self.next_round_delivered = self.delivered;
        }
        if self.delivered >= self.next_round_delivered {
            self.round_count += 1;
            // Next boundary: the delivery count as of the packets in flight now.
            // With no prior_delivered available, the BDP is a usable stand-in.
            let advance = if sample.prior_delivered > 0 {
                sample.prior_delivered
            } else {
                (self.get_bdp() as u64).max(acked_len as u64)
            };
            self.next_round_delivered = self.delivered + advance;
        }

        // ── 4. Enter ProbeRtt once RTprop goes stale ─────────────────────────
        if self.state != BbrState::ProbeRtt && (now - self.rtprop_stamp) > RTPROP_FILTER_LEN {
            self.state = BbrState::ProbeRtt;
            self.probe_rtt_done_stamp = Some(now + PROBE_RTT_DURATION);
        }

        // ── 5. State transitions ─────────────────────────────────────────────
        match self.state {
            BbrState::Startup => {
                // Exit per BBR v1 §4.3.1: three rounds without 25% bandwidth
                // growth, measured against the best rate seen so far.
                self.check_startup_full_bandwidth(sample.delivery_rate);
                if self.startup_full_bw_count >= STARTUP_FULL_BW_COUNT_THRESHOLD {
                    self.state = BbrState::Drain;
                    // Drain paces at the reciprocal of the Startup gain.
                    self.pacing_gain = BBR_DRAIN_GAIN; // ≈ 363/1024 ≈ 0.354x
                    self.cwnd_gain = BBR_HIGH_GAIN; // cwnd stays high while draining
                    self.startup_full_bw_count = 0;
                }
            }
            BbrState::Drain => {
                // Leave once the queue is drained, i.e. inflight is back to BDP.
                if self.inflight <= self.get_bdp() {
                    self.enter_probe_bw(now);
                }
            }
            BbrState::ProbeBw => {
                // Advance the phase at most once per RTT (BBR v1 §4.3.2), using
                // RTprop as the RTT, or 100 ms until the first sample lands.
                let rtprop_us = self.rtprop.get();
                let phase_duration = if rtprop_us > 0 {
                    Duration::from_micros(rtprop_us)
                } else {
                    Duration::from_millis(100)
                };
                if now - self.cycle_stamp >= phase_duration {
                    self.cycle_index = (self.cycle_index + 1) % 8;
                    self.pacing_gain = match self.cycle_index {
                        0 => BBR_PROBE_GAIN,       // 1.25x — probe-up
                        1 => BBR_PROBE_DRAIN_GAIN, // 0.8x  — drain
                        _ => BBR_UNIT,             // 1.0x  — cruise
                    };
                    self.cycle_stamp = now;
                }
            }
            BbrState::ProbeRtt => {
                if let Some(done) = self.probe_rtt_done_stamp {
                    if now >= done {
                        // Restart the RTprop filter window from here.
                        self.rtprop_stamp = now;
                        self.probe_rtt_done_stamp = None;
                        self.enter_probe_bw(now);
                    }
                }
            }
        }

        // ── 6. Recompute cwnd and pacing_rate ────────────────────────────────
        self.update_control_parameters();
    }

    // Karn's algorithm, applied to rate sampling: RTT and rate data drawn across
    // a retransmit are not trustworthy, so the tracker restarts here.
    fn on_retransmit(&mut self, now: Instant) {
        // Reset delivered_time so the next rate sample is not overstated.
        self.delivered_time = now;
        // Reset the round boundary; rounds are counted afresh after a retransmit.
        self.next_round_delivered = self.delivered;
    }

    fn on_duplicate_ack(&mut self, _now: Instant) {}
    fn set_remote_window(&mut self, _window: usize) {}
    fn pre_transmit(&mut self, _now: Instant) {}

    fn post_transmit(&mut self, _now: Instant, bytes: usize) {
        self.inflight += bytes;
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::tcp::RateSample;
    use crate::time::{Duration, Instant};

    fn make_sample(delivery_rate: u64, rtt_ms: u64) -> RateSample {
        RateSample {
            delivery_rate,
            rtt: Some(Duration::from_millis(rtt_ms)),
            is_app_limited: false,
            prior_delivered: 0,
            ..Default::default()
        }
    }

    // The BDP is in bytes; it must not be scaled down by 1024.
    #[test]
    fn test_bdp_units_correct() {
        let mut bbr = Bbr::new();
        let t0 = Instant::from_millis(0);

        // bw = 1_000_000 bytes/s, rtt = 100 ms = 100_000 µs
        // BDP = 1_000_000 * 100_000 / 1_000_000 = 100_000 bytes
        let sample = make_sample(1_000_000, 100);
        bbr.on_ack_with_rate(t0, 1460, &Default::default(), sample);

        let bdp = bbr.get_bdp();
        assert!(
            bdp > 50_000 && bdp < 200_000,
            "BDP={bdp}, expected ~100_000 bytes"
        );
    }

    // With a 10-round window, RTprop must not follow larger samples.
    #[test]
    fn test_rtprop_filter_holds_minimum() {
        let mut bbr = Bbr::new();
        let t0 = Instant::from_millis(0);

        // First measurement: RTT = 50 ms.
        let s = make_sample(1_000_000, 50);
        bbr.on_ack_with_rate(t0, 1460, &Default::default(), s);
        let min_after_first = bbr.rtprop.get();
        assert_eq!(
            min_after_first, 50_000,
            "the first measurement sets the minimum"
        );

        // Five larger samples follow; the minimum must survive them.
        for i in 1..6usize {
            let s = make_sample(1_000_000, 100);
            bbr.on_ack_with_rate(
                Instant::from_millis(i as i64 * 100),
                1460,
                &Default::default(),
                s,
            );
        }
        assert_eq!(
            bbr.rtprop.get(),
            50_000,
            "Minmax must keep the minimum, not the latest value"
        );
    }

    // Startup must not end on the first BDP estimate.
    #[test]
    fn test_startup_does_not_exit_immediately() {
        let mut bbr = Bbr::new();
        let t0 = Instant::from_millis(0);

        // Simulate fast bandwidth growth: +50% per round.
        let mut bw = 100_000u64;
        for i in 0..5usize {
            let s = make_sample(bw, 20);
            bbr.on_ack_with_rate(
                Instant::from_millis(i as i64 * 20),
                1460,
                &Default::default(),
                s,
            );
            bw = bw * 3 / 2; // +50% growth
            assert_eq!(
                bbr.state,
                BbrState::Startup,
                "must stay in Startup while bandwidth grows fast (round {i})"
            );
        }
    }

    // Startup must end after three rounds of slow growth.
    #[test]
    fn test_startup_exits_after_slow_growth() {
        let mut bbr = Bbr::new();
        // prior_delivered = acked gives exactly one round per call: delivered
        // grows by acked each call and advance = prior_delivered = acked, so
        // next_round = old_delivered + acked = new_delivered, and the following
        // call is immediately at the boundary again.
        let acked = 1460usize;
        let high_bw = 4_000_000u64; // sets a high full_bw bar
        let low_bw = 1_000_000u64; // < 1.25 * high_bw, i.e. no growth

        let make_round_sample = |bw: u64, rtt_ms: u64| RateSample {
            delivery_rate: bw,
            rtt: Some(Duration::from_millis(rtt_ms)),
            is_app_limited: false,
            prior_delivered: acked as u64, // one round per ACK
            ..Default::default()
        };

        // Round 1: full_bw = high_bw, count = 0 (first sample).
        bbr.on_ack_with_rate(Instant::from_millis(0), acked, &Default::default(), make_round_sample(high_bw, 20));
        assert_eq!(bbr.state, BbrState::Startup, "round 1: still in Startup");

        // Round 2: high_bw again, no 25% growth, count = 1.
        bbr.on_ack_with_rate(Instant::from_millis(20), acked, &Default::default(), make_round_sample(high_bw, 20));
        assert_eq!(bbr.state, BbrState::Startup, "round 2: still in Startup (count=1)");

        // Round 3: low_bw, count = 2.
        bbr.on_ack_with_rate(Instant::from_millis(40), acked, &Default::default(), make_round_sample(low_bw, 20));
        assert_eq!(bbr.state, BbrState::Startup, "round 3: still in Startup (count=2)");

        // Round 4: low_bw, count = 3, Startup ends.
        bbr.on_ack_with_rate(Instant::from_millis(60), acked, &Default::default(), make_round_sample(low_bw, 20));
        assert_ne!(
            bbr.state,
            BbrState::Startup,
            "Startup must end once bandwidth growth stalls"
        );
    }

    // ProbeRtt lasts exactly 200 ms, not RTT + 200 ms.
    #[test]
    fn test_probe_rtt_duration_is_fixed_200ms() {
        let mut bbr = Bbr::new();
        // Force entry into ProbeRtt.
        bbr.state = BbrState::ProbeRtt;
        let t0 = Instant::from_millis(0);
        bbr.probe_rtt_done_stamp = Some(t0 + PROBE_RTT_DURATION);

        // Before 200 ms: still probing.
        let s = make_sample(0, 100);
        bbr.on_ack_with_rate(Instant::from_millis(150), 0, &Default::default(), s);
        assert_eq!(
            bbr.state,
            BbrState::ProbeRtt,
            "must stay in ProbeRtt before 200 ms"
        );

        // After 200 ms: done.
        bbr.on_ack_with_rate(Instant::from_millis(201), 0, &Default::default(), s);
        assert_ne!(
            bbr.state,
            BbrState::ProbeRtt,
            "must leave ProbeRtt after 200 ms"
        );
    }

    // round_count must advance over time.
    #[test]
    fn test_round_count_increments() {
        let mut bbr = Bbr::new();
        let initial_round = bbr.round_count;

        for i in 0..20usize {
            let s = RateSample {
                delivery_rate: 1_000_000,
                rtt: Some(Duration::from_millis(20)),
                is_app_limited: false,
                prior_delivered: (i as u64) * 1460,
                ..Default::default()
            };
            bbr.on_ack_with_rate(
                Instant::from_millis(i as i64 * 20),
                1460,
                &Default::default(),
                s,
            );
        }
        assert!(
            bbr.round_count > initial_round,
            "round_count must advance: got {}",
            bbr.round_count
        );
    }

    // pacing_rate must land in a plausible range.
    #[test]
    fn test_pacing_rate_reasonable() {
        let mut bbr = Bbr::new();
        let t0 = Instant::from_millis(0);

        // bw = 10 Mbit/s = 1_250_000 bytes/s
        let s = make_sample(1_250_000, 50);
        bbr.on_ack_with_rate(t0, 10_000, &Default::default(), s);

        // In Startup, pacing_rate = BW * HIGH_GAIN ≈ 2.885x
        // = 1_250_000 * 2885 / 1024 ≈ 3_521_972
        let pr = bbr.pacing_rate;
        assert!(
            pr > 1_000_000 && pr < 10_000_000,
            "pacing_rate={pr} is out of a plausible range"
        );
    }

    // A huge BtlBw (microsecond-interval sample on a local link) must not overflow
    // the BDP / cwnd / pacing arithmetic.
    #[test]
    fn test_huge_btlbw_does_not_overflow() {
        let mut bbr = Bbr::new();
        bbr.set_tunnel_rtt_override(Some(Duration::from_millis(500)));
        let sample = RateSample {
            delivery_rate: u64::MAX / 3,
            rtt: Some(Duration::from_millis(1)),
            ..Default::default()
        };
        bbr.on_ack_with_rate(Instant::from_millis(0), 1460, &Default::default(), sample);
        assert!(bbr.get_bdp() >= bbr.mss * 4);
        assert!(bbr.window() >= bbr.mss * 4);
        let _ = bbr.pacing_rate();
    }
}
