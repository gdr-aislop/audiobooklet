//! The download speed shown on the Downloads tab and on the progress strip under a book.
//!
//! `DownloadEvent::TrackProgress` carries the cumulative bytes of *one track*, and up to two
//! tracks of a book download at once, so their events alternate. Subtracting "the last number
//! seen" from the next one, whichever track it belongs to, read a 50 MB track followed by a 1 MB
//! one as 49 MB in a few milliseconds: hundreds of MB/s on a link doing 3. The meter keeps the
//! last value per track and counts only each track's own forward progress. A track's first
//! report counts nothing, since a resumed one starts at its saved offset rather than at zero.
//! The rate is the bytes moved over a short sliding window, so two events landing a few
//! milliseconds apart can't inflate it.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// How far back the rate looks.
const WINDOW: Duration = Duration::from_secs(3);
/// Less than this much time between the oldest and newest sample in the window is too little to
/// divide by: no rate yet.
const MIN_SPAN: Duration = Duration::from_millis(1000);
/// A newest sample older than this means nothing has arrived in a while: hide the rate rather than
/// show a frozen one as if it were current.
const STALE_AFTER: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct SpeedMeter {
    /// The latest cumulative bytes each track (by `ino`) reported.
    tracks: HashMap<String, u64>,
    /// Bytes moved since the first sample, for the samples inside the window.
    samples: VecDeque<(Instant, u64)>,
    moved: u64,
}

impl SpeedMeter {
    /// Records a track's cumulative byte count, as reported at `now`.
    pub(crate) fn record(&mut self, ino: &str, bytes: u64, now: Instant) {
        match self.tracks.insert(ino.to_string(), bytes) {
            // First report of this track: a baseline, not progress.
            None => {}
            // A lower number is a track that started over: re-baselined, nothing moved.
            Some(previous) => self.moved += bytes.saturating_sub(previous),
        }
        self.samples.push_back((now, self.moved));
        while self.samples.len() > 1 && now.duration_since(self.samples[0].0) > WINDOW {
            self.samples.pop_front();
        }
    }

    /// The bytes the tracks have reported between them: each track's latest value.
    pub(crate) fn total(&self) -> u64 {
        self.tracks.values().sum()
    }

    /// Bytes per second over the recent window, or `None` while there's too little data or the
    /// newest sample is stale.
    pub(crate) fn rate(&self, now: Instant) -> Option<f64> {
        let (newest_at, newest) = *self.samples.back()?;
        if now.saturating_duration_since(newest_at) > STALE_AFTER {
            return None;
        }
        let (oldest_at, oldest) = *self.samples.front()?;
        let span = newest_at.duration_since(oldest_at);
        if span < MIN_SPAN {
            return None;
        }
        let rate = newest.saturating_sub(oldest) as f64 / span.as_secs_f64();
        (rate > 0.0).then_some(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// One track at a steady 3 MB/s, reported every 200 ms.
    #[test]
    fn a_steady_single_track_reads_its_real_speed() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        for step in 0..=15u64 {
            meter.record("a", step * 600_000, at(t0, step * 200));
        }
        let rate = meter.rate(at(t0, 3_000)).unwrap();
        assert!((rate - 3.0 * MB as f64).abs() < 0.1 * MB as f64, "{rate}");
    }

    /// The reported bug: two tracks alternating, one far ahead of the other. Together they move
    /// 3 MB/s; the old code read the alternation as hundreds.
    #[test]
    fn two_tracks_downloading_at_once_add_up_instead_of_spiking() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        for step in 0..=30u64 {
            // Each track gets half the link: 1.5 MB/s, 300 KB per 200 ms event. "a" is 50 MB in.
            meter.record("a", 50 * MB + step * 300_000, at(t0, step * 200));
            meter.record("b", MB + step * 300_000, at(t0, step * 200 + 5));
            if step > 6 {
                let rate = meter.rate(at(t0, step * 200 + 5)).unwrap();
                assert!(rate < 3.5 * MB as f64, "step {step}: {rate}");
            }
        }
        let rate = meter.rate(at(t0, 6_005)).unwrap();
        assert!((rate - 3.0 * MB as f64).abs() < 0.2 * MB as f64, "{rate}");
        assert_eq!(meter.total(), 51 * MB + 2 * 30 * 300_000);
    }

    /// A track resumed from a 100 MB checkpoint reports 100 MB on its first event: not progress.
    #[test]
    fn a_resumed_track_does_not_count_its_saved_offset() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        for step in 0..=10u64 {
            meter.record("a", 100 * MB + step * 600_000, at(t0, step * 200));
        }
        let rate = meter.rate(at(t0, 2_000)).unwrap();
        assert!((rate - 3.0 * MB as f64).abs() < 0.1 * MB as f64, "{rate}");
    }

    /// A track that starts over (lower number) re-baselines instead of going negative or
    /// counting the whole file again.
    #[test]
    fn a_track_starting_over_is_a_new_baseline() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        meter.record("a", 80 * MB, t0);
        meter.record("a", 80 * MB + 600_000, at(t0, 200));
        meter.record("a", 0, at(t0, 400));
        for step in 3..=12u64 {
            meter.record("a", (step - 2) * 600_000, at(t0, step * 200));
        }
        let rate = meter.rate(at(t0, 2_400)).unwrap();
        assert!(rate < 3.5 * MB as f64, "{rate}");
    }

    /// Two events a few milliseconds apart (the main loop delivering a batch at once) must not
    /// turn one chunk into a huge rate.
    #[test]
    fn events_a_few_milliseconds_apart_do_not_inflate_the_rate() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        for step in 0..=10u64 {
            let base = step * 200;
            meter.record("a", step * 600_000, at(t0, base));
            // Delivered late, right behind the previous one.
            meter.record("a", step * 600_000 + 16_000, at(t0, base + 2));
        }
        let rate = meter.rate(at(t0, 2_002)).unwrap();
        assert!(rate < 3.5 * MB as f64, "{rate}");
    }

    #[test]
    fn no_rate_until_there_is_enough_data_and_none_when_stale() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        assert_eq!(meter.rate(t0), None, "no samples");
        meter.record("a", 0, t0);
        meter.record("a", 600_000, at(t0, 200));
        assert_eq!(meter.rate(at(t0, 200)), None, "under a second of data");
        for step in 2..=10u64 {
            meter.record("a", step * 600_000, at(t0, step * 200));
        }
        assert!(meter.rate(at(t0, 2_000)).is_some());
        assert_eq!(meter.rate(at(t0, 4_500)), None, "nothing has arrived for over 2 s");
    }

    /// A stalled link (bytes stop moving) reads as no rate rather than a stale number.
    #[test]
    fn a_stalled_transfer_has_no_rate() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::default();
        for step in 0..=15u64 {
            meter.record("a", 5 * MB, at(t0, step * 200));
        }
        assert_eq!(meter.rate(at(t0, 3_000)), None);
    }
}
