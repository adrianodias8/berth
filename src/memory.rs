//! Memory reclaiming: steer the VM's balloon so the guest only holds what it
//! actually uses plus headroom, and memory it frees goes back to macOS.
//!
//! Every couple of seconds we read the guest's MemTotal/MemAvailable via the
//! agent. MemAvailable counts reclaimable page cache, so shrinking towards
//! "used + headroom" also squeezes out stale cache. We shrink gently and grow
//! at once; when the agent's PSI trigger reports memory stalls, we hand back
//! the full allowance immediately and hold off shrinking for a while.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::daemon::guest_stream;
use crate::vm;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Never squeeze the guest below this.
const FLOOR: u64 = GIB;
/// At most this much is taken back per tick, so the guest can drop cache calmly.
const SHRINK_STEP: u64 = GIB;
/// Ignore changes smaller than this to avoid churn.
const HYSTERESIS: u64 = 256 * MIB;
const TICK: Duration = Duration::from_secs(2);
/// After a pressure event, don't shrink again for this long.
const HOLD_OFF: Duration = Duration::from_secs(30);

/// Guest memory as reported by the agent. `total` still includes balloon
/// pages (VZ negotiates deflate-on-OOM, so the guest keeps counting them), so
/// everything below works on `total - balloon`.
#[derive(Deserialize, Debug, Clone, Copy)]
pub struct Sample {
    pub total: u64,
    pub available: u64,
    pub pressure: f64,
    #[serde(default)]
    pub balloon: u64,
}

impl Sample {
    /// Memory the guest can actually use right now.
    fn usable(&self) -> u64 {
        self.total.saturating_sub(self.balloon)
    }

    /// In use by the guest, excluding reclaimable cache and the balloon.
    pub fn used(&self) -> u64 {
        self.usable().saturating_sub(self.available)
    }
}

/// Guest bytes in use (excluding reclaimable cache), for `berth status`.
pub static GUEST_USED: AtomicU64 = AtomicU64::new(0);
/// Current balloon target, i.e. memory the VM may hold.
pub static TARGET: AtomicU64 = AtomicU64::new(0);
static HOLD_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Where the balloon should go next. `max` is the configured VM size,
/// `current` the present target.
pub fn next_target(max: u64, current: u64, s: &Sample) -> u64 {
    let used = s.used();
    // Under pressure or nearly out of room: everything back, now.
    if s.pressure > 2.0 || s.available < s.usable() / 8 {
        return max;
    }
    let headroom = GIB.max(used / 2);
    let want = (used + headroom).clamp(FLOOR.min(max), max);
    let next = if want > current {
        // Grow at once, with a little extra so we don't creep up tick by tick.
        (want + GIB).min(max)
    } else {
        want.max(current.saturating_sub(SHRINK_STEP))
    };
    let next = next / MIB * MIB;
    if next.abs_diff(current) < HYSTERESIS && next != max { current } else { next }
}

/// Called when the agent reports memory stalls.
pub async fn on_pressure(max: u64) {
    *HOLD_UNTIL.lock().unwrap() = Some(Instant::now() + HOLD_OFF);
    if TARGET.load(Ordering::Relaxed) < max && vm::set_balloon_target(max).await.is_ok() {
        TARGET.store(max, Ordering::Relaxed);
        eprintln!("memory: pressure in guest, returned full {} MiB", max / MIB);
    }
}

pub async fn run(max: u64) {
    TARGET.store(max, Ordering::Relaxed);
    loop {
        tokio::time::sleep(TICK).await;
        let Ok(sample) = sample().await else { continue };
        GUEST_USED.store(sample.used(), Ordering::Relaxed);
        let current = TARGET.load(Ordering::Relaxed);
        let mut next = next_target(max, current, &sample);
        if next < current && HOLD_UNTIL.lock().unwrap().is_some_and(|t| Instant::now() < t) {
            next = current;
        }
        if next != current && vm::set_balloon_target(next).await.is_ok() {
            TARGET.store(next, Ordering::Relaxed);
        }
    }
}

async fn sample() -> anyhow::Result<Sample> {
    let mut guest = guest_stream("info memory").await?;
    let mut json = String::new();
    guest.read_to_string(&mut json).await?;
    Ok(serde_json::from_str(&json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u64 = 16 * GIB;

    fn s(total: u64, available: u64) -> Sample {
        Sample { total, available, pressure: 0.0, balloon: 0 }
    }

    #[test]
    fn shrinks_gradually_towards_use_plus_headroom() {
        // 16 GiB VM, 500 MiB in use: one step down per tick.
        let t = next_target(MAX, MAX, &s(MAX, MAX - 500 * MIB));
        assert_eq!(t, MAX - GIB);
        // Eventually settles at used + 1 GiB headroom (above the floor).
        let mut cur = MAX;
        for _ in 0..40 {
            cur = next_target(MAX, cur, &s(cur, cur - 500 * MIB));
        }
        assert_eq!(cur, 1524 * MIB);
    }

    #[test]
    fn grows_at_once_when_usage_rises() {
        // Squeezed to 4 GiB, guest now uses 3: want 3 + 1.5 headroom, plus
        // 1 GiB extra on growth.
        let t = next_target(MAX, 2 * GIB, &s(4 * GIB, GIB));
        assert_eq!(t, 5 * GIB + 512 * MIB);
    }

    #[test]
    fn pressure_returns_everything() {
        let mut sample = s(2 * GIB, GIB);
        sample.pressure = 10.0;
        assert_eq!(next_target(MAX, 2 * GIB, &sample), MAX);
        assert_eq!(next_target(MAX, 2 * GIB, &s(2 * GIB, 100 * MIB)), MAX);
    }

    #[test]
    fn balloon_pages_are_not_usage() {
        // The bug this guards against: 16 GiB guest, 6 GiB in the balloon,
        // 9 GiB available. Real use is 1 GiB, not 7.
        let sample = Sample { total: MAX, available: 9 * GIB, pressure: 0.0, balloon: 6 * GIB };
        assert_eq!(sample.used(), GIB);
        // So with 10 GiB allowed we keep shrinking, rather than growing back.
        assert!(next_target(MAX, 10 * GIB, &sample) < 10 * GIB);
    }

    #[test]
    fn small_changes_are_ignored() {
        let cur = 3 * GIB;
        // want = 1.9 + 1 = 2.9 GiB: within hysteresis of 3 GiB.
        assert_eq!(next_target(MAX, cur, &s(cur, cur - 1946 * MIB)), cur);
    }
}
