//! Lightweight opt-in render profiling for local performance investigations.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENV_VAR: &str = "HERDR_RENDER_PROF";

static ENABLED: OnceLock<bool> = OnceLock::new();
static PROFILER: OnceLock<Mutex<RenderProfiler>> = OnceLock::new();

#[derive(Default)]
struct DurationStats {
    count: u64,
    total_ns: u128,
    max_ns: u128,
}

struct RenderProfiler {
    window_started: Instant,
    counters: BTreeMap<&'static str, u64>,
    pane_counters: BTreeMap<(u32, &'static str), u64>,
    durations: BTreeMap<&'static str, DurationStats>,
    pane_durations: BTreeMap<(u32, &'static str), DurationStats>,
}

impl RenderProfiler {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            counters: BTreeMap::new(),
            pane_counters: BTreeMap::new(),
            durations: BTreeMap::new(),
            pane_durations: BTreeMap::new(),
        }
    }

    fn increment(&mut self, name: &'static str, value: u64) {
        *self.counters.entry(name).or_default() += value;
    }

    fn increment_pane(&mut self, pane_id: u32, name: &'static str, value: u64) {
        *self.pane_counters.entry((pane_id, name)).or_default() += value;
    }

    fn duration(&mut self, name: &'static str, duration: Duration) {
        let stats = self.durations.entry(name).or_default();
        let ns = duration.as_nanos();
        stats.count += 1;
        stats.total_ns += ns;
        stats.max_ns = stats.max_ns.max(ns);
    }

    fn pane_duration(&mut self, pane_id: u32, name: &'static str, duration: Duration) {
        let stats = self.pane_durations.entry((pane_id, name)).or_default();
        let ns = duration.as_nanos();
        stats.count += 1;
        stats.total_ns += ns;
        stats.max_ns = stats.max_ns.max(ns);
    }

    fn flush_if_due(&mut self) {
        let elapsed = self.window_started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let counters = self
            .counters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let pane_counters = self
            .pane_counters
            .iter()
            .map(|((pane_id, name), value)| format!("pane.{pane_id}.{name}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let durations = self
            .durations
            .iter()
            .map(|(name, stats)| {
                let avg_us = if stats.count == 0 {
                    0
                } else {
                    stats.total_ns / u128::from(stats.count) / 1_000
                };
                let max_us = stats.max_ns / 1_000;
                format!(
                    "{name}=count:{} avg_us:{} max_us:{}",
                    stats.count, avg_us, max_us
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let pane_durations = self
            .pane_durations
            .iter()
            .map(|((pane_id, name), stats)| {
                let avg_us = if stats.count == 0 {
                    0
                } else {
                    stats.total_ns / u128::from(stats.count) / 1_000
                };
                let max_us = stats.max_ns / 1_000;
                format!(
                    "pane.{pane_id}.{name}=count:{} avg_us:{} max_us:{}",
                    stats.count, avg_us, max_us
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        tracing::info!(
            event = "render.prof",
            window_ms = elapsed.as_millis() as u64,
            counters = %counters,
            pane_counters = %pane_counters,
            durations = %durations,
            pane_durations = %pane_durations,
            "render profiler window"
        );

        self.window_started = Instant::now();
        self.counters.clear();
        self.pane_counters.clear();
        self.durations.clear();
        self.pane_durations.clear();
    }
}

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var(ENV_VAR)
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    })
}

fn with_profiler(update: impl FnOnce(&mut RenderProfiler)) {
    if !enabled() {
        return;
    }
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    if let Ok(mut profiler) = profiler.lock() {
        update(&mut profiler);
    }
}

pub(crate) fn counter(name: &'static str, value: u64) {
    if value == 0 {
        return;
    }
    with_profiler(|profiler| profiler.increment(name, value));
}

pub(crate) fn pane_counter(pane_id: crate::layout::PaneId, name: &'static str, value: u64) {
    if value == 0 {
        return;
    }
    with_profiler(|profiler| profiler.increment_pane(pane_id.raw(), name, value));
}

pub(crate) fn event(name: &'static str) {
    counter(name, 1);
}

pub(crate) fn pane_event(pane_id: crate::layout::PaneId, name: &'static str) {
    pane_counter(pane_id, name, 1);
}

pub(crate) fn duration(name: &'static str, duration: Duration) {
    with_profiler(|profiler| profiler.duration(name, duration));
}

pub(crate) fn pane_duration(
    pane_id: crate::layout::PaneId,
    name: &'static str,
    duration: Duration,
) {
    with_profiler(|profiler| profiler.pane_duration(pane_id.raw(), name, duration));
}

pub(crate) fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn duration_since(name: &'static str, started: Option<Instant>) {
    if let Some(started) = started {
        duration(name, started.elapsed());
    }
}

pub(crate) fn pane_duration_since(
    pane_id: crate::layout::PaneId,
    name: &'static str,
    started: Option<Instant>,
) {
    if let Some(started) = started {
        pane_duration(pane_id, name, started.elapsed());
    }
}

pub(crate) fn flush_if_due() {
    with_profiler(RenderProfiler::flush_if_due);
}
