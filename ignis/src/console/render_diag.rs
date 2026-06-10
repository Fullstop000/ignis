//! Render-loop health instrumentation.
//!
//! Inline rendering paints the conversation *only* by committing rows into the
//! terminal's native scrollback (`Terminal::insert_before`), and only after a
//! successful viewport re-anchor. The live band never draws conversation
//! content. So when the re-anchor stalls — the DSR (`ESC[6n`) cursor query can
//! lag indefinitely under output backpressure on WSL2/conpty — the commit loop
//! is gated and the screen goes blank while the agent keeps working. That
//! failure used to be completely silent.
//!
//! `RenderDiag` records the render loop's vital signs (frames, commits,
//! re-anchor success/failure, forced re-anchors) and, with `IGNIS_LOG_RENDER=1`,
//! logs a periodic heartbeat to `~/.ignis/logs/ignis.log`. The choke-point
//! events (re-anchor stalls, forced recoveries) are logged unconditionally by
//! the runner regardless of this flag — they're rare and always worth a record.

use std::time::{Duration, Instant};

/// How often the opt-in heartbeat summarizes render-loop health.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) struct RenderDiag {
    verbose: bool,
    last_heartbeat: Instant,
    // Lifetime tallies since launch.
    frames: u64,
    commit_batches: u64,
    rows_committed: u64,
    reanchor_ok: u64,
    reanchor_failed: u64,
    forced_reanchors: u64,
}

impl RenderDiag {
    /// Build from the environment. `IGNIS_LOG_RENDER=1` enables the heartbeat.
    pub(crate) fn from_env() -> Self {
        let verbose = std::env::var("IGNIS_LOG_RENDER")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false);
        if verbose {
            log::info!("render: IGNIS_LOG_RENDER on — render-health heartbeat every 5s");
        }
        Self {
            verbose,
            last_heartbeat: Instant::now(),
            frames: 0,
            commit_batches: 0,
            rows_committed: 0,
            reanchor_ok: 0,
            reanchor_failed: 0,
            forced_reanchors: 0,
        }
    }

    pub(crate) fn on_frame(&mut self) {
        self.frames += 1;
    }

    pub(crate) fn on_commit(&mut self, rows: usize) {
        self.commit_batches += 1;
        self.rows_committed += rows as u64;
    }

    pub(crate) fn on_reanchor_ok(&mut self) {
        self.reanchor_ok += 1;
    }

    pub(crate) fn on_reanchor_failed(&mut self) {
        self.reanchor_failed += 1;
    }

    pub(crate) fn on_forced_reanchor(&mut self) {
        self.forced_reanchors += 1;
    }

    /// Emit a heartbeat line if verbose and the interval has elapsed. Cheap to
    /// call every loop iteration — it's just an `Instant::elapsed` check until
    /// the interval is due.
    pub(crate) fn heartbeat(&mut self) {
        if !self.verbose || self.last_heartbeat.elapsed() < HEARTBEAT_INTERVAL {
            return;
        }
        self.last_heartbeat = Instant::now();
        log::info!(
            "render: frames={} commits={} rows={} reanchor_ok={} reanchor_failed={} forced={}",
            self.frames,
            self.commit_batches,
            self.rows_committed,
            self.reanchor_ok,
            self.reanchor_failed,
            self.forced_reanchors,
        );
    }
}
