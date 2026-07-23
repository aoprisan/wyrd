//! Chaos mode: seeded schedule perturbation to flush out latent races.
//!
//! A concurrency bug that survives your test suite is usually protected by
//! timing: the race window is a few microseconds wide and your scheduler
//! happens to never land in it. Chaos mode deliberately widens those windows
//! by injecting small, pseudo-random delays (or bare yields) right before the
//! shim's acquisition points — lock/read/write/acquire/send — and at task
//! startup. The delays come from a seeded generator, so a seed that provokes
//! a deadlock keeps provoking it, run after run.
//!
//! Chaos is configured entirely through the environment, so any
//! shim-instrumented binary becomes fuzzable without a rebuild:
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `WYRD_CHAOS` | `1`/`true`/`on` enables chaos | off |
//! | `WYRD_CHAOS_SEED` | seed for the delay stream | `0x5EED` |
//! | `WYRD_CHAOS_PROB` | per-site injection probability (0..=1) | `0.25` |
//! | `WYRD_CHAOS_MAX_DELAY_US` | max injected delay, µs (0 = yield only) | `500` |
//!
//! `wyrd hunt` drives exactly these variables across many child runs and
//! reports which seeds made the app hang, deadlock, or invert lock order.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// Chaos parameters. See the module docs for the matching env variables.
#[derive(Debug, Clone, PartialEq)]
pub struct ChaosConfig {
    /// Seed for the pseudo-random delay stream.
    pub seed: u64,
    /// Probability (0..=1) that any given chaos point injects a delay.
    pub probability: f64,
    /// Upper bound for an injected delay. Zero means chaos points only
    /// `yield_now`, never sleep.
    pub max_delay: Duration,
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self {
            seed: 0x5EED,
            probability: 0.25,
            max_delay: Duration::from_micros(500),
        }
    }
}

impl ChaosConfig {
    /// Read the `WYRD_CHAOS*` variables; `None` unless `WYRD_CHAOS` is set to
    /// `1`/`true`/`on`. Malformed values fall back to the defaults.
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("WYRD_CHAOS")
            .map(|v| matches!(v.trim(), "1" | "true" | "on"))
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("WYRD_CHAOS_SEED") {
            if let Ok(seed) = v.trim().parse::<u64>() {
                cfg.seed = seed;
            }
        }
        if let Ok(v) = std::env::var("WYRD_CHAOS_PROB") {
            if let Ok(p) = v.trim().parse::<f64>() {
                if (0.0..=1.0).contains(&p) {
                    cfg.probability = p;
                }
            }
        }
        if let Ok(v) = std::env::var("WYRD_CHAOS_MAX_DELAY_US") {
            if let Ok(us) = v.trim().parse::<u64>() {
                cfg.max_delay = Duration::from_micros(us);
            }
        }
        Some(cfg)
    }
}

struct ChaosState {
    cfg: ChaosConfig,
    /// Draw counter: each chaos point consumes one index of the seeded
    /// stream. The mapping of indices to sites depends on scheduling, so this
    /// is perturbation, not replay — but a fixed seed still explores a stable
    /// neighbourhood of schedules.
    counter: AtomicU64,
}

static CHAOS: OnceLock<Option<ChaosState>> = OnceLock::new();

/// Enable chaos with an explicit config (first call wins; [`crate::init`] and
/// [`crate::init_from_env`] call the env-driven variant automatically).
pub fn init_chaos(cfg: ChaosConfig) {
    let _ = CHAOS.set(Some(ChaosState {
        cfg,
        counter: AtomicU64::new(0),
    }));
}

/// Resolve chaos from the environment exactly once. Called by the recorder
/// initializers so `WYRD_CHAOS=1` works on any shim-instrumented binary
/// without code changes.
pub(crate) fn init_chaos_from_env() {
    let _ = CHAOS.get_or_init(|| {
        ChaosConfig::from_env().map(|cfg| ChaosState {
            cfg,
            counter: AtomicU64::new(0),
        })
    });
}

/// The active chaos config, if chaos is enabled.
pub fn chaos_config() -> Option<ChaosConfig> {
    CHAOS.get().and_then(|s| s.as_ref()).map(|s| s.cfg.clone())
}

/// SplitMix64: a tiny, high-quality mixer — one draw per chaos point.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// A chaos point: maybe yield or sleep, per the seeded stream. Free when
/// chaos is disabled. Placed immediately before the shim's acquisition
/// operations and at task startup.
pub(crate) async fn chaos_point() {
    let Some(state) = CHAOS.get().and_then(|s| s.as_ref()) else {
        return;
    };
    let n = state.counter.fetch_add(1, Ordering::Relaxed);
    let r = splitmix64(state.cfg.seed.wrapping_add(n));
    // Top 53 bits → uniform in [0, 1).
    let roll = (r >> 11) as f64 / (1u64 << 53) as f64;
    if roll >= state.cfg.probability {
        return;
    }
    let max_us = state.cfg.max_delay.as_micros() as u64;
    let delay_us = if max_us == 0 {
        0
    } else {
        splitmix64(r) % (max_us + 1)
    };
    if delay_us == 0 {
        tokio::task::yield_now().await;
    } else {
        tokio::time::sleep(Duration::from_micros(delay_us)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_stream_is_deterministic() {
        let a: Vec<u64> = (0..8).map(|i| splitmix64(42u64.wrapping_add(i))).collect();
        let b: Vec<u64> = (0..8).map(|i| splitmix64(42u64.wrapping_add(i))).collect();
        assert_eq!(a, b);
        // And actually varies.
        assert!(a.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn env_parsing_defaults_and_overrides() {
        // No WYRD_CHAOS → disabled. (Set/unset is process-global; keep this
        // test single-threaded with respect to these variables.)
        std::env::remove_var("WYRD_CHAOS");
        assert!(ChaosConfig::from_env().is_none());

        std::env::set_var("WYRD_CHAOS", "1");
        std::env::set_var("WYRD_CHAOS_SEED", "7");
        std::env::set_var("WYRD_CHAOS_PROB", "0.5");
        std::env::set_var("WYRD_CHAOS_MAX_DELAY_US", "100");
        let cfg = ChaosConfig::from_env().expect("enabled");
        assert_eq!(cfg.seed, 7);
        assert_eq!(cfg.probability, 0.5);
        assert_eq!(cfg.max_delay, Duration::from_micros(100));

        std::env::remove_var("WYRD_CHAOS");
        std::env::remove_var("WYRD_CHAOS_SEED");
        std::env::remove_var("WYRD_CHAOS_PROB");
        std::env::remove_var("WYRD_CHAOS_MAX_DELAY_US");
    }
}
