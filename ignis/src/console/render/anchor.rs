//! Pure state machine for the inline viewport's anchoring lifecycle.
//!
//! The inline TUI keeps a fixed-height live band pinned at the bottom of the
//! normal buffer and pushes finished rows into native scrollback via
//! `insert_before`. Three delicate dances around that band used to live as
//! loose flags and event-loop locals in `runner.rs`, and produced a string of
//! field bugs (#138, #140, #154, #155). They're modeled here as one pure,
//! clock-injected machine so each invariant is a table test instead of a
//! dogfood discovery:
//!
//! 1. **Screen-clear re-anchor episodes** (`/clear`, `/resume`): the screen
//!    must be wiped exactly once per episode, commits must stay gated until
//!    the viewport re-anchors (#140), and a stalled DSR must trigger a
//!    DSR-free fallback after a bounded number of attempts instead of wedging
//!    rendering blank (#154).
//! 2. **Resize settle** (#138): a beat after the last `Event::Resize` the band
//!    forces one full wipe + re-anchor to scrub conpty's late-reflow
//!    duplicates; the settle marker is consumed only by a *settle* re-anchor
//!    that landed, never by mid-drag rebuilds or a timed-out DSR.
//! 3. **Commit row budget** (#155): a single `insert_before` batch must stay
//!    under ratatui's `u16` cell limit, so a tall block splits across frames
//!    with a cursor marking the split point.
//!
//! The machine is two-phase at each decision point: a `*_step` method returns
//! what the runner should do (wipe kind + rebuild height), the runner performs
//! the terminal IO, then reports the outcome back via `*_rebuilt`. No IO, no
//! wall clock — `now` is a caller-supplied monotonic offset.

use std::time::Duration;

/// Consecutive failed re-anchor attempts (each blocks ~the crossterm DSR
/// timeout, ~2s) before giving up on the DSR and re-anchoring without one.
/// Bounds the blank window to a few seconds — the "blank after input, full
/// content on resume" wedge (#154).
pub(crate) const MAX_REANCHOR_ATTEMPTS: u32 = 2;

/// Quiet period after the last `Event::Resize` before the settle re-anchor
/// fires. Lets a slow terminal (conpty/WT over WSL2) finish reflowing before
/// the repaint; tune here if duplicates survive a monitor drag (#138).
pub(crate) const RESIZE_SETTLE: Duration = Duration::from_millis(250);

/// How much of the terminal a re-anchor must wipe before rebuilding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wipe {
    /// Visible screen + scrollback (`Clear(All)` + `Clear(Purge)` + home).
    /// Used by screen-clear episodes and resize recovery so stale history or
    /// inline-band rows can't linger when scrolling up.
    All,
    /// ratatui `Terminal::clear()` — non-destructive band cleanup. Used while
    /// resize geometry is still moving and when only band height changes.
    Band,
}

/// One re-anchor instruction: wipe (maybe), then rebuild the inline viewport
/// at `want_rows` and report the result back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reanchor {
    pub wipe: Option<Wipe>,
    pub want_rows: u16,
}

/// Outcome of reporting a screen-clear episode rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearOutcome {
    /// Re-anchor landed; commits resume. `attempts` failed tries preceded it.
    Landed { attempts: u32, waited: Duration },
    /// DSR didn't answer; episode stays pending, commits stay gated.
    Held { attempts: u32, waited: Duration },
    /// DSR unresponsive after `MAX_REANCHOR_ATTEMPTS`: the caller must
    /// re-anchor WITHOUT a DSR (ratatui `clear()`), and commits resume so the
    /// gate can't wedge rendering blank indefinitely.
    ForcedFallback { attempts: u32, waited: Duration },
}

/// Pure anchoring state machine. Owns every flag the inline render loop used
/// to scatter across `App` fields and event-loop locals.
#[derive(Debug)]
pub(crate) struct Anchor {
    /// A screen-clear re-anchor episode is pending; commits are gated.
    pending_clear: bool,
    /// When the current episode's wipe was issued (`None` = not yet wiped
    /// this episode). Keyed off so the screen is cleared ONCE per episode —
    /// re-wiping every frame floods the terminal and worsens the very DSR
    /// backpressure that keeps the re-anchor from landing.
    episode_started: Option<Duration>,
    /// Failed rebuild attempts within the current episode.
    attempts: u32,
    /// Time of the most recent `Event::Resize`; cleared only by a settle
    /// re-anchor that landed.
    last_resize: Option<Duration>,
    /// Set by `band_step` when the pending rebuild is a settle re-anchor, so
    /// `band_rebuilt` knows whether success consumes the resize marker.
    settle_in_flight: bool,
    /// Height of the inline viewport the terminal is currently built at.
    pub(crate) viewport_rows: u16,
    /// Last adopted terminal size, to detect resizes (ratatui 0.26 inline
    /// doesn't repaint cleanly across them).
    term_size: (u16, u16),
}

impl Anchor {
    pub(crate) fn new(viewport_rows: u16, term_size: (u16, u16)) -> Self {
        Self {
            pending_clear: false,
            episode_started: None,
            attempts: 0,
            last_resize: None,
            settle_in_flight: false,
            viewport_rows,
            term_size,
        }
    }

    /// A session reset (`/clear`, `/resume`) wants the screen wiped and the
    /// viewport re-anchored. A request arriving mid-episode joins the episode
    /// already in flight rather than restarting the wipe.
    pub(crate) fn request_reanchor(&mut self) {
        if self.pending_clear {
            return;
        }
        self.pending_clear = true;
        self.episode_started = None;
        self.attempts = 0;
    }

    /// An `Event::Resize` arrived (including same-grid-size DPI changes,
    /// which only the event — not the reported size — can see).
    pub(crate) fn on_resize(&mut self, now: Duration) {
        self.last_resize = Some(now);
    }

    /// Whether blocks may flow into scrollback this frame. False while a
    /// screen-clear episode is unresolved: committing first would advance the
    /// cursor into blocks the wipe then erases — paint once, get wiped,
    /// never repaint (#140).
    pub(crate) fn can_commit(&self) -> bool {
        !self.pending_clear
    }

    /// The screen-clear episode decision, run every frame before commits.
    /// `Some` = wipe (first frame of the episode only) and attempt a rebuild
    /// at `want_rows`, then report via [`Self::clear_rebuilt`].
    pub(crate) fn clear_step(&mut self, now: Duration, want_rows: u16) -> Option<Reanchor> {
        if !self.pending_clear {
            return None;
        }
        let wipe = if self.episode_started.is_none() {
            self.episode_started = Some(now);
            self.attempts = 0;
            Some(Wipe::All)
        } else {
            None
        };
        Some(Reanchor { wipe, want_rows })
    }

    /// Report the rebuild attempt for a [`Self::clear_step`]. On success the
    /// band adopts `want_rows`/`term_size`; on `ForcedFallback` geometry is
    /// left untouched so the draw-time check converges it on a later,
    /// non-gating rebuild.
    pub(crate) fn clear_rebuilt(
        &mut self,
        ok: bool,
        now: Duration,
        want_rows: u16,
        term_size: (u16, u16),
    ) -> ClearOutcome {
        let waited = self
            .episode_started
            .map(|t| now.saturating_sub(t))
            .unwrap_or_default();
        if ok {
            let attempts = self.attempts;
            self.end_episode();
            self.viewport_rows = want_rows;
            self.term_size = term_size;
            return ClearOutcome::Landed { attempts, waited };
        }
        self.attempts += 1;
        let attempts = self.attempts;
        if attempts >= MAX_REANCHOR_ATTEMPTS {
            self.end_episode();
            ClearOutcome::ForcedFallback { attempts, waited }
        } else {
            ClearOutcome::Held { attempts, waited }
        }
    }

    fn end_episode(&mut self) {
        self.pending_clear = false;
        self.episode_started = None;
        self.attempts = 0;
    }

    /// The draw-section band geometry decision, run once per frame interval.
    /// Fires when the band height needs to change, the terminal size changed,
    /// or a resize settled. Live size changes use a non-destructive `Band`
    /// rebuild so a drag does not repeatedly purge/replay history. The one
    /// post-burst settle rebuild escalates to `All` to remove any band rows the
    /// terminal reflowed into scrollback.
    pub(crate) fn band_step(
        &mut self,
        now: Duration,
        want_rows: u16,
        term_size: (u16, u16),
    ) -> Option<Reanchor> {
        let size_changed = term_size != self.term_size;
        // Some terminals can expose the new grid size before (or without)
        // delivering Event::Resize. Treat the observed geometry change as a
        // resize episode so it still gets one settle cleanup.
        if size_changed {
            self.last_resize = Some(now);
        }
        let settled = self
            .last_resize
            .is_some_and(|t| now.saturating_sub(t) >= RESIZE_SETTLE);
        if want_rows == self.viewport_rows && !size_changed && !settled {
            return None;
        }
        self.settle_in_flight = settled;
        let wipe = if settled { Wipe::All } else { Wipe::Band };
        Some(Reanchor {
            wipe: Some(wipe),
            want_rows,
        })
    }

    /// Report the rebuild attempt for a [`Self::band_step`]. Success adopts
    /// the new geometry and — only for a settle re-anchor — consumes the
    /// resize marker. Failure (DSR timeout) leaves everything in place so the
    /// next frame retries; a timed-out settle keeps the marker (#138).
    pub(crate) fn band_rebuilt(&mut self, ok: bool, want_rows: u16, term_size: (u16, u16)) {
        if ok {
            self.viewport_rows = want_rows;
            self.term_size = term_size;
            if self.settle_in_flight {
                self.last_resize = None;
            }
        }
        self.settle_in_flight = false;
    }
}

/// Row-budget arithmetic for one block in the per-frame commit batch (#155):
/// from a block render `rows_len` tall, with `committed_rows` already in
/// scrollback and `budget` rows left in this frame's batch, take
/// `start..start + take`. `drained` = the block's final row lands this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommitTake {
    pub start: usize,
    pub take: usize,
    pub drained: bool,
}

pub(crate) fn commit_take(rows_len: usize, committed_rows: usize, budget: usize) -> CommitTake {
    let start = committed_rows.min(rows_len);
    let take = (rows_len - start).min(budget);
    CommitTake {
        start,
        take,
        drained: start + take == rows_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u16, u16) = (80, 24);
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- screen-clear episodes: commit gate (#140) ----

    #[test]
    fn commits_flow_until_a_reanchor_is_requested() {
        let mut a = Anchor::new(8, SIZE);
        assert!(a.can_commit());
        a.request_reanchor();
        assert!(!a.can_commit(), "pending episode must gate commits (#140)");
    }

    #[test]
    fn commits_stay_gated_while_dsr_holds_and_resume_on_landing() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();

        let step = a.clear_step(ms(0), 8).expect("episode must re-anchor");
        assert_eq!(step.want_rows, 8);
        let out = a.clear_rebuilt(false, ms(0), 8, SIZE);
        assert!(matches!(out, ClearOutcome::Held { attempts: 1, .. }));
        assert!(!a.can_commit(), "held episode keeps the gate closed");

        a.clear_step(ms(100), 8).expect("episode still pending");
        let out = a.clear_rebuilt(true, ms(100), 8, SIZE);
        assert!(matches!(out, ClearOutcome::Landed { attempts: 1, .. }));
        assert!(a.can_commit(), "landed re-anchor reopens the gate");
        assert_eq!(a.clear_step(ms(200), 8), None, "episode is over");
    }

    // ---- screen-clear episodes: wipe-once + bounded attempts (#154) ----

    #[test]
    fn screen_is_wiped_exactly_once_per_episode() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();

        let first = a.clear_step(ms(0), 8).unwrap();
        assert_eq!(
            first.wipe,
            Some(Wipe::All),
            "first frame of an episode wipes screen + scrollback"
        );
        a.clear_rebuilt(false, ms(0), 8, SIZE);

        let second = a.clear_step(ms(50), 8).unwrap();
        assert_eq!(
            second.wipe, None,
            "retries within an episode must NOT re-wipe — flooding the \
             terminal worsens the DSR backpressure (#154)"
        );
    }

    #[test]
    fn second_request_mid_episode_joins_without_rewiping() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();
        a.clear_step(ms(0), 8).unwrap();
        a.clear_rebuilt(false, ms(0), 8, SIZE);

        a.request_reanchor(); // e.g. /clear racing a /resume episode
        let step = a.clear_step(ms(50), 8).unwrap();
        assert_eq!(step.wipe, None, "mid-episode request joins the episode");
    }

    #[test]
    fn stalled_dsr_forces_a_dsr_free_fallback_after_max_attempts() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();

        for attempt in 1..MAX_REANCHOR_ATTEMPTS {
            a.clear_step(ms(0), 8).unwrap();
            let out = a.clear_rebuilt(false, ms(0), 8, SIZE);
            assert!(
                matches!(out, ClearOutcome::Held { attempts, .. } if attempts == attempt),
                "attempt {attempt} below the bound holds"
            );
        }

        a.clear_step(ms(0), 8).unwrap();
        let out = a.clear_rebuilt(false, ms(4000), 8, SIZE);
        assert!(
            matches!(
                out,
                ClearOutcome::ForcedFallback {
                    attempts: MAX_REANCHOR_ATTEMPTS,
                    ..
                }
            ),
            "the gate must not wedge rendering blank indefinitely (#154), got {out:?}"
        );
        assert!(a.can_commit(), "fallback reopens the gate");
        assert_eq!(a.clear_step(ms(4100), 8), None, "episode is over");
    }

    #[test]
    fn forced_fallback_leaves_band_geometry_untouched() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();
        for _ in 0..MAX_REANCHOR_ATTEMPTS {
            a.clear_step(ms(0), 12).unwrap();
            a.clear_rebuilt(false, ms(0), 12, SIZE);
        }
        assert_eq!(
            a.viewport_rows, 8,
            "fallback keeps the old viewport so the draw-time check converges \
             the band on a later, non-gating rebuild"
        );
        // ...and that later band rebuild is indeed offered:
        assert!(a.band_step(ms(100), 12, SIZE).is_some());
    }

    #[test]
    fn landed_reanchor_adopts_band_geometry() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();
        a.clear_step(ms(0), 12).unwrap();
        a.clear_rebuilt(true, ms(0), 12, (100, 30));
        assert_eq!(a.viewport_rows, 12);
        assert_eq!(
            a.band_step(ms(50), 12, (100, 30)),
            None,
            "adopted geometry needs no draw-time rebuild"
        );
    }

    #[test]
    fn held_episode_reports_elapsed_wait() {
        let mut a = Anchor::new(8, SIZE);
        a.request_reanchor();
        a.clear_step(ms(1000), 8).unwrap();
        match a.clear_rebuilt(false, ms(3000), 8, SIZE) {
            ClearOutcome::Held { waited, .. } => assert_eq!(waited, ms(2000)),
            out => panic!("expected Held, got {out:?}"),
        }
    }

    // ---- resize settle (#138) ----

    #[test]
    fn band_is_idle_when_geometry_converged() {
        let mut a = Anchor::new(8, SIZE);
        assert_eq!(a.band_step(ms(0), 8, SIZE), None);
    }

    #[test]
    fn band_height_change_rebuilds_with_band_only_wipe() {
        let mut a = Anchor::new(8, SIZE);
        // Picker opened: band grows, terminal size unchanged.
        let step = a.band_step(ms(0), 14, SIZE).unwrap();
        assert_eq!(step.wipe, Some(Wipe::Band));
        assert_eq!(step.want_rows, 14);
        a.band_rebuilt(true, 14, SIZE);
        assert_eq!(a.viewport_rows, 14);
        assert_eq!(a.band_step(ms(33), 14, SIZE), None);
    }

    #[test]
    fn size_change_rebuilds_without_purging_scrollback() {
        let mut a = Anchor::new(8, SIZE);
        let step = a.band_step(ms(0), 8, (120, 40)).unwrap();
        assert_eq!(
            step.wipe,
            Some(Wipe::Band),
            "drag-time geometry changes must not purge and replay scrollback"
        );
    }

    #[test]
    fn same_size_resize_fires_full_wipe_only_after_settle() {
        // Cross-DPI monitor drag: Event::Resize fires but the grid size is
        // unchanged, so only the settle pass can scrub the duplicates (#138).
        let mut a = Anchor::new(8, SIZE);
        a.on_resize(ms(0));
        assert_eq!(
            a.band_step(ms(100), 8, SIZE),
            None,
            "before the quiet period nothing fires"
        );
        let step = a.band_step(ms(250), 8, SIZE).expect("settle fires");
        assert_eq!(
            step.wipe,
            Some(Wipe::All),
            "the settle pass must purge late-reflow duplicates from scrollback"
        );
        a.band_rebuilt(true, 8, SIZE);
        assert_eq!(
            a.band_step(ms(300), 8, SIZE),
            None,
            "a landed settle consumes the resize marker"
        );
    }

    #[test]
    fn timed_out_settle_keeps_the_marker_for_retry() {
        let mut a = Anchor::new(8, SIZE);
        a.on_resize(ms(0));
        a.band_step(ms(250), 8, SIZE).expect("settle fires");
        a.band_rebuilt(false, 8, SIZE); // DSR timeout (common on WSL2/conpty)
        assert!(
            a.band_step(ms(300), 8, SIZE).is_some(),
            "a timed-out settle re-anchor must retry next frame (#138)"
        );
    }

    #[test]
    fn mid_drag_rebuild_does_not_consume_the_settle_marker() {
        let mut a = Anchor::new(8, SIZE);
        a.on_resize(ms(0));
        // Live size-change rebuild during the drag, before the quiet period.
        let step = a.band_step(ms(50), 8, (120, 40)).unwrap();
        assert_eq!(step.wipe, Some(Wipe::Band));
        a.band_rebuilt(true, 8, (120, 40));
        // The settle must STILL fire after the quiet period — consuming the
        // marker on a drag rebuild would mean it never does (#138).
        let step = a
            .band_step(ms(300), 8, (120, 40))
            .expect("settle still fires");
        assert_eq!(step.wipe, Some(Wipe::All));
    }

    #[test]
    fn resize_burst_has_one_destructive_settle_rebuild() {
        let mut a = Anchor::new(8, SIZE);

        a.on_resize(ms(0));
        let first = a.band_step(ms(10), 8, (100, 30)).unwrap();
        assert_eq!(first.wipe, Some(Wipe::Band));
        a.band_rebuilt(true, 8, (100, 30));

        a.on_resize(ms(100));
        let second = a.band_step(ms(110), 8, (120, 35)).unwrap();
        assert_eq!(second.wipe, Some(Wipe::Band));
        a.band_rebuilt(true, 8, (120, 35));

        assert_eq!(
            a.band_step(ms(300), 8, (120, 35)),
            None,
            "the final resize event restarts the quiet period"
        );
        let settle = a.band_step(ms(360), 8, (120, 35)).unwrap();
        assert_eq!(settle.wipe, Some(Wipe::All));
        a.band_rebuilt(true, 8, (120, 35));

        assert_eq!(
            a.band_step(ms(400), 8, (120, 35)),
            None,
            "a landed settle cleanup runs only once per burst"
        );
    }

    #[test]
    fn observed_size_changes_restart_settle_without_resize_events() {
        let mut a = Anchor::new(8, SIZE);

        let first = a.band_step(ms(0), 8, (100, 30)).unwrap();
        assert_eq!(first.wipe, Some(Wipe::Band));
        a.band_rebuilt(true, 8, (100, 30));

        let second = a.band_step(ms(200), 8, (120, 35)).unwrap();
        assert_eq!(second.wipe, Some(Wipe::Band));
        a.band_rebuilt(true, 8, (120, 35));

        assert_eq!(
            a.band_step(ms(250), 8, (120, 35)),
            None,
            "a later observed size must restart the quiet period"
        );
        let settle = a.band_step(ms(450), 8, (120, 35)).unwrap();
        assert_eq!(settle.wipe, Some(Wipe::All));
    }

    #[test]
    fn new_resize_during_pending_settle_extends_the_quiet_period() {
        let mut a = Anchor::new(8, SIZE);
        a.on_resize(ms(0));
        a.on_resize(ms(200)); // drag continues
        assert_eq!(
            a.band_step(ms(300), 8, SIZE),
            None,
            "quiet period restarts from the latest resize"
        );
        assert!(a.band_step(ms(450), 8, SIZE).is_some());
    }

    // ---- commit row budget (#155) ----

    #[test]
    fn block_within_budget_drains_in_one_frame() {
        assert_eq!(
            commit_take(10, 0, 100),
            CommitTake {
                start: 0,
                take: 10,
                drained: true
            }
        );
    }

    #[test]
    fn tall_block_splits_across_frames_at_the_budget() {
        // Frame 1: budget caps the take; the block is NOT drained.
        assert_eq!(
            commit_take(900, 0, 819),
            CommitTake {
                start: 0,
                take: 819,
                drained: false
            }
        );
        // Frame 2: resumes at the split point and drains.
        assert_eq!(
            commit_take(900, 819, 819),
            CommitTake {
                start: 819,
                take: 81,
                drained: true
            }
        );
    }

    #[test]
    fn streaming_block_takes_only_newly_settled_rows() {
        // 5 rows already in scrollback, 7 stable now → take the 2 new ones.
        assert_eq!(
            commit_take(7, 5, 100),
            CommitTake {
                start: 5,
                take: 2,
                drained: true
            }
        );
    }

    #[test]
    fn cursor_is_clamped_when_a_rerender_shrinks_the_block() {
        // A re-render can yield fewer rows than were committed (width change
        // mid-stream): clamp instead of panicking on the slice.
        assert_eq!(
            commit_take(3, 5, 100),
            CommitTake {
                start: 3,
                take: 0,
                drained: true
            }
        );
    }

    #[test]
    fn exhausted_budget_takes_nothing_but_keeps_position() {
        assert_eq!(
            commit_take(10, 4, 0),
            CommitTake {
                start: 4,
                take: 0,
                drained: false
            }
        );
    }
}
