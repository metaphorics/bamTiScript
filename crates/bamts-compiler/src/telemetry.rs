//! Zero-cost, thread-local phase telemetry for the frontend pipeline.
//!
//! Telemetry is opt-in per thread. When no [`TelemetryCollector`] is active on
//! the current thread, [`Telemetry::measure`] runs its closure behind a single
//! [`Cell`] read: no [`Instant`] is sampled, no [`Duration`] accumulates, and no
//! allocation occurs, so the disabled path adds no observable overhead beyond
//! that one branch. When a collector is active, each measured phase's wall time
//! accumulates into a thread-local total that [`TelemetryCollector::snapshot`]
//! reads.
//!
//! The pipeline records one wall for each of [`Phase::Scan`], [`Phase::Parse`],
//! [`Phase::Bind`], [`Phase::Check`], and [`Phase::Emit`], plus [`Phase::Total`]
//! for the whole call. A phase the pipeline never times stays at zero rather
//! than being omitted, so downstream consumers always see the complete key set.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::time::{Duration, Instant};

/// A frontend compilation phase whose wall time telemetry attributes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Phase {
    /// Lexical scanning of one source.
    Scan,
    /// Parsing scanned tokens into a recovered tree.
    Parse,
    /// Binding (folded into [`Phase::Check`] by the current checker; recorded
    /// separately so the key set stays stable if binding is ever split out).
    Bind,
    /// Type checking (currently also performs binding).
    Check,
    /// Emitting the checked module.
    Emit,
    /// The whole `compile_*_frontend_with_lints` call.
    Total,
}

/// Accumulated per-phase wall time for one collection scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhaseTotals {
    /// Scanning wall time.
    pub scan: Duration,
    /// Parsing wall time.
    pub parse: Duration,
    /// Binding wall time (zero while binding is folded into checking).
    pub bind: Duration,
    /// Checking wall time.
    pub check: Duration,
    /// Emitting wall time.
    pub emit: Duration,
    /// Whole-call wall time.
    pub total: Duration,
}

impl PhaseTotals {
    /// All-zero totals: the canonical "no telemetry collected" value.
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            scan: Duration::ZERO,
            parse: Duration::ZERO,
            bind: Duration::ZERO,
            check: Duration::ZERO,
            emit: Duration::ZERO,
            total: Duration::ZERO,
        }
    }

    /// The accumulated wall time for `phase`.
    #[must_use]
    pub const fn get(&self, phase: Phase) -> Duration {
        match phase {
            Phase::Scan => self.scan,
            Phase::Parse => self.parse,
            Phase::Bind => self.bind,
            Phase::Check => self.check,
            Phase::Emit => self.emit,
            Phase::Total => self.total,
        }
    }

    /// The accumulated wall time for `phase` in fractional milliseconds.
    #[must_use]
    pub fn millis(&self, phase: Phase) -> f64 {
        self.get(phase).as_secs_f64() * 1_000.0
    }

    fn add(&mut self, phase: Phase, elapsed: Duration) {
        let slot = match phase {
            Phase::Scan => &mut self.scan,
            Phase::Parse => &mut self.parse,
            Phase::Bind => &mut self.bind,
            Phase::Check => &mut self.check,
            Phase::Emit => &mut self.emit,
            Phase::Total => &mut self.total,
        };
        *slot = slot.saturating_add(elapsed);
    }
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static TOTALS: RefCell<PhaseTotals> = const { RefCell::new(PhaseTotals::zero()) };
}

/// The thread-local phase telemetry namespace.
pub struct Telemetry;

impl Telemetry {
    /// The canonical empty telemetry value, for callers that ran with no
    /// collector active.
    #[must_use]
    pub const fn disabled() -> PhaseTotals {
        PhaseTotals::zero()
    }

    /// Whether a [`TelemetryCollector`] is active on the current thread.
    #[must_use]
    pub fn enabled() -> bool {
        ACTIVE.with(Cell::get)
    }

    /// Runs `f`, attributing its wall time to `phase` when telemetry is active.
    ///
    /// When inactive this is a single [`Cell`] read followed by the call, with
    /// no timing and no accumulation.
    pub fn measure<T>(phase: Phase, f: impl FnOnce() -> T) -> T {
        if !Self::enabled() {
            return f();
        }
        let start = Instant::now();
        let value = f();
        let elapsed = start.elapsed();
        TOTALS.with(|totals| totals.borrow_mut().add(phase, elapsed));
        value
    }
}

/// Enables thread-local phase telemetry for the lifetime of this guard.
///
/// Construction resets the thread-local totals; [`TelemetryCollector::snapshot`]
/// reads the running total; dropping the guard disables collection again.
/// Collectors do not nest — constructing one while another is active on the same
/// thread panics — so every measured wall attributes to exactly one scope.
pub struct TelemetryCollector {
    // Telemetry lives in thread-local storage, so a collector must not move to
    // another thread and disable the wrong thread's flag on drop.
    _not_send: PhantomData<*const ()>,
}

impl TelemetryCollector {
    /// Enables telemetry on the current thread, resetting the accumulators.
    ///
    /// # Panics
    /// Panics if a collector is already active on the current thread.
    #[must_use]
    pub fn start() -> Self {
        assert!(
            !Telemetry::enabled(),
            "telemetry collectors do not nest on one thread",
        );
        TOTALS.with(|totals| *totals.borrow_mut() = PhaseTotals::zero());
        ACTIVE.with(|active| active.set(true));
        Self {
            _not_send: PhantomData,
        }
    }

    /// The phase totals accumulated on this thread since this guard started.
    #[must_use]
    pub fn snapshot(&self) -> PhaseTotals {
        TOTALS.with(|totals| *totals.borrow())
    }
}

impl Drop for TelemetryCollector {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::{Phase, PhaseTotals, Telemetry, TelemetryCollector};
    use std::time::Duration;

    #[test]
    fn disabled_measure_runs_closure_without_collecting() {
        assert!(!Telemetry::enabled());
        let mut ran = false;
        let out = Telemetry::measure(Phase::Parse, || {
            ran = true;
            7
        });
        assert!(ran);
        assert_eq!(out, 7);
        assert!(!Telemetry::enabled());
        assert_eq!(Telemetry::disabled(), PhaseTotals::zero());
    }

    #[test]
    fn active_collector_accumulates_measured_phase() {
        let collector = TelemetryCollector::start();
        assert!(Telemetry::enabled());
        Telemetry::measure(Phase::Check, || {
            std::thread::sleep(Duration::from_millis(2));
        });
        let snapshot = collector.snapshot();
        assert!(snapshot.check > Duration::ZERO, "check wall was recorded");
        assert!(snapshot.millis(Phase::Check) > 0.0);
        // Unmeasured phases stay at zero rather than being dropped.
        assert_eq!(snapshot.parse, Duration::ZERO);
        assert_eq!(snapshot.bind, Duration::ZERO);
        drop(collector);
        assert!(!Telemetry::enabled());
    }

    #[test]
    fn nested_phases_attribute_independently() {
        let collector = TelemetryCollector::start();
        Telemetry::measure(Phase::Total, || {
            Telemetry::measure(Phase::Scan, || std::thread::sleep(Duration::from_millis(1)));
            Telemetry::measure(Phase::Parse, || {
                std::thread::sleep(Duration::from_millis(1))
            });
        });
        let snapshot = collector.snapshot();
        assert!(snapshot.scan > Duration::ZERO);
        assert!(snapshot.parse > Duration::ZERO);
        // Total encloses both sub-phases, so it is at least as large as either.
        assert!(snapshot.total >= snapshot.scan);
        assert!(snapshot.total >= snapshot.parse);
    }

    #[test]
    fn dropping_collector_disables_and_a_new_one_resets() {
        {
            let first = TelemetryCollector::start();
            Telemetry::measure(Phase::Emit, || std::thread::sleep(Duration::from_millis(1)));
            assert!(first.snapshot().emit > Duration::ZERO);
        }
        assert!(!Telemetry::enabled());
        let second = TelemetryCollector::start();
        assert_eq!(second.snapshot(), PhaseTotals::zero());
    }

    #[test]
    #[should_panic(expected = "telemetry collectors do not nest")]
    fn collectors_do_not_nest() {
        let _outer = TelemetryCollector::start();
        let _inner = TelemetryCollector::start();
    }
}
