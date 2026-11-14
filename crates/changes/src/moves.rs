//! A partition's move to another writer group, as every store makes one
//! (step 3d; spec/a-deployment-scales-out-and-fails-over-by-configuration
//! section 6.8): its bounds from the chart, CI's injections, and who asked.
//! The book's moves (`bor`) and the lake's ranges' (`lake`) each keep their
//! own warm, gate, hold and switch.

use std::path::PathBuf;
use std::time::Duration;

/// The retry-after a write held past the hold is answered with.
pub const HELD_RETRY_SECONDS: u32 = 1;

/// The hold's default, `moves.holdSeconds` (section 6.8).
pub const HOLD_SECONDS: u64 = 5;

/// The variable the chart sets from `moves.holdSeconds` (approved
/// 2026-11-14).
pub const HOLD_SECONDS_VAR: &str = "MERIDIAN_MOVES_HOLD_SECONDS";

/// How long a write held by a move waits for its switch (section 6.8, the
/// hold): `moves.holdSeconds` as the chart sets it, else its default.
pub fn hold() -> Duration {
    static HOLD: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *HOLD.get_or_init(|| {
        Duration::from_secs(
            std::env::var(HOLD_SECONDS_VAR)
                .ok()
                .and_then(|said| said.trim().parse().ok())
                .unwrap_or(HOLD_SECONDS),
        )
    })
}

/// `moves.switchSeconds`'s default (section 6.8, Vitess's `--timeout`).
pub const SWITCH_SECONDS: u64 = 30;

/// The variable the chart sets from `moves.switchSeconds` (approved
/// 2026-11-14).
pub const SWITCH_SECONDS_VAR: &str = "MERIDIAN_MOVES_SWITCH_SECONDS";

/// CI's injected lag (approved 2026-11-14, CI only): a file whose presence
/// is a lag -- its number of milliseconds, else one past every bound.
pub const LAG_FILE_VAR: &str = "MERIDIAN_MOVE_LAG_FILE";

/// CI's injected digest mismatch (approved 2026-11-14, CI only): a file
/// whose presence makes the target's digest at the switch differ.
pub const DIGEST_FILE_VAR: &str = "MERIDIAN_MOVE_DIGEST_FILE";

/// A move's bounds, from the chart.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `moves.switchSeconds`: the gate's and the switch's bound each.
    pub switch: Duration,
    /// `<store>.readyLagMs`: the lag the gate opens under.
    pub ready_lag: Duration,
    /// CI's injected lag, where set.
    pub lag_file: Option<PathBuf>,
    /// CI's injected digest mismatch, where set.
    pub digest_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            switch: Duration::from_secs(SWITCH_SECONDS),
            ready_lag: Duration::from_millis(5_000),
            lag_file: None,
            digest_file: None,
        }
    }
}

impl Settings {
    /// As the chart sets them; each default where unset.
    pub fn from_env() -> Self {
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|said| said.trim().parse::<u64>().ok())
        };
        let file = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|said| !said.trim().is_empty())
                .map(PathBuf::from)
        };
        let fallback = Self::default();
        Self {
            switch: number(SWITCH_SECONDS_VAR)
                .map(Duration::from_secs)
                .unwrap_or(fallback.switch),
            ready_lag: number("MERIDIAN_READY_LAG_MS")
                .map(Duration::from_millis)
                .unwrap_or(fallback.ready_lag),
            lag_file: file(LAG_FILE_VAR),
            digest_file: file(DIGEST_FILE_VAR),
        }
    }

    /// The lag CI injects, where its file is there.
    pub fn injected_lag(&self) -> Option<Duration> {
        let path = self.lag_file.as_ref()?;
        let said = std::fs::read_to_string(path).ok()?;
        Some(
            said.trim()
                .parse::<u64>()
                .map(Duration::from_millis)
                .unwrap_or(Duration::MAX),
        )
    }

    /// Whether CI makes the digests differ.
    pub fn digest_differs(&self) -> bool {
        self.digest_file.as_ref().is_some_and(|path| path.exists())
    }
}

/// Who asked, and why: empty for a move by a changed count.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub by_hand: bool,
    pub moved_by: String,
    pub reason: String,
}

/// A digest's first bytes, as a finding names it.
pub fn short(digest: &[u8]) -> String {
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}
