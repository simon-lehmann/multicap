//! Turning the timestamps the compositor gave us into a cadence an editor can cut.
//!
//! This module is plain arithmetic and is unit-tested on any platform. It exists
//! because capture timestamps are honest but untidy: they wobble by around 10 ms,
//! and a game nominally at 60 fps drifts between 59.9 and 60.6. Written into a
//! file unchanged, an NLE decimating to 30 fps picks frames unevenly and the
//! motion judders.
//!
//! The fix is to trust the timestamps in the large and not in the small: pick
//! anchors a couple of seconds apart, keep their real times, and space the frames
//! between them evenly. Long-run drift is preserved; per-frame jitter is not.

/// 100 ns ticks in one second: the unit Windows timestamps and Media Foundation
/// samples both use.
pub const TICKS_PER_SEC: i64 = 10_000_000;

/// Space `times` evenly between anchors roughly `anchor_secs` apart.
///
/// Returns a vector the same length as `times`. The first and last timestamps are
/// always preserved exactly, as is every anchor in between.
pub fn even_out(times: &[i64], anchor_secs: f64) -> Vec<i64> {
    if times.len() < 3 {
        return times.to_vec();
    }
    let step = (anchor_secs * TICKS_PER_SEC as f64) as i64;
    let mut out = vec![0i64; times.len()];

    // Anchor indices: the first frame, then the first frame at or past each
    // multiple of `step`, then the last frame.
    let mut anchors = vec![0usize];
    let mut next = times[0] + step;
    for (i, &t) in times.iter().enumerate() {
        if t >= next {
            if i > *anchors.last().unwrap() {
                anchors.push(i);
            }
            next = t + step;
        }
    }
    let last = times.len() - 1;
    if *anchors.last().unwrap() != last {
        anchors.push(last);
    }

    for w in anchors.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (ta, tb) = (times[a], times[b]);
        let span = (b - a) as i64;
        for (k, slot) in out[a..=b].iter_mut().enumerate() {
            *slot = ta + (tb - ta) * k as i64 / span;
        }
    }
    out
}

/// What the cadence of a take actually was. Written into the take's sidecar so a
/// bad take is caught by its numbers rather than by eye.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cadence {
    pub frames: u64,
    pub seconds: f64,
    /// Frames divided by elapsed time: the real rate, not the nominal one.
    pub fps: f64,
    /// Gaps longer than 1.5x the median interval, as (second, length) pairs.
    /// A gap inside a moving shot is a reason to shoot the take again.
    pub gaps: Vec<(f64, f64)>,
    /// Frames the encoder could not take in time. Must be 0 in a usable take.
    pub dropped: u64,
}

pub fn cadence(times: &[i64], dropped: u64) -> Cadence {
    if times.len() < 2 {
        return Cadence {
            frames: times.len() as u64,
            seconds: 0.0,
            fps: 0.0,
            gaps: vec![],
            dropped,
        };
    }
    let span = times[times.len() - 1] - times[0];
    let seconds = span as f64 / TICKS_PER_SEC as f64;

    let mut deltas: Vec<i64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sorted = deltas.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];

    let threshold = median * 3 / 2;
    let gaps = deltas
        .drain(..)
        .enumerate()
        .filter(|&(_, d)| d > threshold)
        .map(|(i, d)| {
            (
                (times[i] - times[0]) as f64 / TICKS_PER_SEC as f64,
                d as f64 / TICKS_PER_SEC as f64,
            )
        })
        .collect();

    Cadence {
        frames: times.len() as u64,
        seconds,
        fps: (times.len() - 1) as f64 / seconds,
        gaps,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 fps with the jitter a real capture shows.
    fn jittery(n: usize) -> Vec<i64> {
        let nominal = TICKS_PER_SEC / 60;
        (0..n)
            .map(|i| {
                let wobble = ((i * 7919) % 200) as i64 - 100; // +/- 100 us, deterministic
                i as i64 * nominal + wobble * 10
            })
            .collect()
    }

    #[test]
    fn evening_keeps_the_ends_exactly() {
        let t = jittery(600);
        let e = even_out(&t, 2.0);
        assert_eq!(e[0], t[0]);
        assert_eq!(e[599], t[599]);
        assert_eq!(e.len(), t.len());
    }

    #[test]
    fn evening_removes_jitter_but_keeps_the_rate() {
        let t = jittery(600);
        let e = even_out(&t, 2.0);

        let spread = |v: &[i64]| {
            let d: Vec<i64> = v.windows(2).map(|w| w[1] - w[0]).collect();
            (*d.iter().max().unwrap() - *d.iter().min().unwrap()) as f64 / TICKS_PER_SEC as f64
        };
        // Jitter collapses by orders of magnitude...
        assert!(spread(&e) < spread(&t) / 10.0, "jitter not reduced");
        // ...while the overall duration is untouched.
        assert_eq!(e[599] - e[0], t[599] - t[0]);
    }

    #[test]
    fn evening_follows_real_drift_rather_than_flattening_it() {
        // A capture that genuinely slows down halfway through must not be
        // "corrected" into a constant rate: the second half really is slower.
        let fast = TICKS_PER_SEC / 60;
        let slow = TICKS_PER_SEC / 30;
        let mut t = vec![0i64];
        for i in 1..300 {
            t.push(t[i - 1] + if i < 150 { fast } else { slow });
        }
        let e = even_out(&t, 2.0);
        let first_half = e[100] - e[99];
        let second_half = e[250] - e[249];
        assert!(
            second_half > first_half * 3 / 2,
            "real drift was flattened: {first_half} vs {second_half}"
        );
    }

    #[test]
    fn short_takes_pass_through_untouched() {
        assert_eq!(even_out(&[5, 10], 2.0), vec![5, 10]);
        assert_eq!(even_out(&[], 2.0), Vec::<i64>::new());
    }

    #[test]
    fn cadence_reports_the_real_rate_and_finds_a_gap() {
        let mut t = jittery(300);
        // A dropped-out stretch: the game did not draw for a quarter second.
        for v in t.iter_mut().skip(150) {
            *v += TICKS_PER_SEC / 4;
        }
        let c = cadence(&t, 0);
        assert_eq!(c.frames, 300);
        // 299 intervals over 5 s of drawing plus a 0.25 s freeze. The rate is
        // reported over wall time on purpose: a take that froze did not run at 60.
        assert!((c.fps - 57.1).abs() < 0.5, "fps was {}", c.fps);
        assert_eq!(
            c.gaps.len(),
            1,
            "expected exactly one gap, got {:?}",
            c.gaps
        );
        assert!((c.gaps[0].1 - 0.2666).abs() < 0.01);
    }
}
