//! Eased scrolling for streaming output: when the content moves up by N lines, it is drawn
//! N lines lower and glides up. The glide is a critically damped spring that keeps its velocity
//! between frames, so bursty output changes the speed gradually instead of in jerks.
//!
//! Only small moves glide. Fast-moving text smears, so a burst of output (or a big jump)
//! scrolls instantly and gliding resumes once the output calms down.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use maomi_term::{CellColor, GridSnapshot, TermMode};

/// Spring stiffness (angular frequency, 1/s); a 1-line glide settles in about 4/OMEGA s.
const OMEGA: f32 = 24.;
/// Offsets (lines) and speeds (lines/s) below these snap to rest.
const REST: f32 = 0.01;
const REST_SPEED: f32 = 0.05;
/// Most lines a glide covers; larger moves skip ahead to this distance.
const MAX_GLIDE: f32 = 3.;
/// More than this many lines within [`BURST_WINDOW`] counts as a burst: scroll instantly.
const BURST_LINES: usize = 6;
const BURST_WINDOW: Duration = Duration::from_millis(300);
/// History this deep may be at the scrollback limit, where its size no longer grows on scroll.
const MAYBE_FULL: usize = 1000;

#[derive(Debug, Default)]
pub(crate) struct SmoothScroll {
    /// Fingerprint of every viewport row in the last frame.
    hashes: Vec<u64>,
    columns: usize,
    history_size: usize,
    /// Lines the content is drawn below its real place.
    offset: f32,
    /// d(offset)/dt in lines per second (negative while gliding up).
    velocity: f32,
    /// Recent scroll moves, for burst detection.
    recent: VecDeque<(Instant, usize)>,
    last_frame: Option<Instant>,
}

impl SmoothScroll {
    /// Advances the ease to `now`, folds in how far `viewport` moved since the last frame and
    /// returns the offset (in lines) to draw it with.
    pub(crate) fn update(&mut self, viewport: &GridSnapshot, now: Instant, enabled: bool) -> f32 {
        if let Some(last) = self.last_frame {
            let dt = now.saturating_duration_since(last).as_secs_f32();
            self.step_spring(dt);
        }
        self.last_frame = Some(now);

        let hashes = row_hashes(viewport);
        let comparable = enabled
            && viewport.display_offset == 0
            && !viewport.mode.contains(TermMode::ALT_SCREEN)
            && viewport.columns == self.columns
            && hashes.len() == self.hashes.len();
        if comparable {
            let shift = self.shift(viewport, &hashes);
            if shift > 0 {
                self.recent.push_back((now, shift));
            }
            while self
                .recent
                .front()
                .is_some_and(|&(t, _)| now.saturating_duration_since(t) > BURST_WINDOW)
            {
                self.recent.pop_front();
            }
            let burst = self.recent.iter().map(|&(_, n)| n).sum::<usize>() > BURST_LINES;
            if burst {
                self.offset = 0.;
                self.velocity = 0.;
            } else {
                self.offset = (self.offset + shift as f32).min(MAX_GLIDE);
            }
        } else {
            self.offset = 0.;
            self.velocity = 0.;
        }
        if self.offset < REST && self.velocity.abs() < REST_SPEED {
            self.offset = 0.;
            self.velocity = 0.;
        }

        self.hashes = hashes;
        self.columns = viewport.columns;
        self.history_size = viewport.history_size;
        self.offset
    }

    pub(crate) fn is_animating(&self) -> bool {
        self.offset > 0.
    }

    /// Exact critically damped spring toward 0: x(t) = (x0 + (v0 + w*x0) t) e^(-w t).
    fn step_spring(&mut self, dt: f32) {
        let (x0, v0) = (self.offset, self.velocity);
        let decay = (-OMEGA * dt).exp();
        let k = v0 + OMEGA * x0;
        self.offset = (x0 + k * dt) * decay;
        self.velocity = (v0 - OMEGA * dt * k) * decay;
        // It can cross zero once when moving fast; it never draws content above its place.
        if self.offset < 0. {
            self.offset = 0.;
            self.velocity = 0.;
        }
    }

    /// Lines the content moved up since the last frame.
    fn shift(&self, viewport: &GridSnapshot, hashes: &[u64]) -> usize {
        // While history grows, its growth is the exact scroll count.
        if viewport.history_size > self.history_size {
            return viewport.history_size - self.history_size;
        }
        // Shrunk or unchanged below the limit: nothing scrolled (or the screen was cleared).
        if viewport.history_size < self.history_size || viewport.history_size < MAYBE_FULL {
            return 0;
        }
        // At the scrollback limit the size stops growing; find the shift from the rows instead.
        let cursor_at_bottom = viewport
            .cursor
            .is_some_and(|c| c.row + 1 >= viewport.rows_above + viewport.screen_lines);
        detect_shift(&self.hashes, hashes).unwrap_or(if cursor_at_bottom {
            viewport.screen_lines
        } else {
            0
        })
    }
}

/// Smallest `k` where the new rows equal the old rows moved up by `k`. The bottom two rows are
/// left out of the comparison because the prompt or a partly written line changes there.
fn detect_shift(old: &[u64], new: &[u64]) -> Option<usize> {
    let n = old.len();
    if n < 4 || new.len() != n {
        return None;
    }
    (0..n - 3).find(|&k| {
        let m = n - k - 2;
        new[..m] == old[k..k + m]
    })
}

fn row_hashes(viewport: &GridSnapshot) -> Vec<u64> {
    viewport
        .rows()
        .skip(viewport.rows_above)
        .map(|row| {
            let mut hasher = std::hash::DefaultHasher::new();
            for cell in row {
                cell.c.hash(&mut hasher);
                hash_color(cell.fg, &mut hasher);
                hash_color(cell.bg, &mut hasher);
                cell.flags.bits().hash(&mut hasher);
            }
            hasher.finish()
        })
        .collect()
}

fn hash_color(color: CellColor, hasher: &mut impl Hasher) {
    match color {
        CellColor::Foreground => hasher.write_u8(0),
        CellColor::Background => hasher.write_u8(1),
        CellColor::Indexed(i) => hasher.write(&[2, i]),
        CellColor::Rgb(rgb) => hasher.write(&[3, rgb.r, rgb.g, rgb.b]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn detects_upward_shift() {
        let old = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(detect_shift(&old, &old), Some(0));
        assert_eq!(detect_shift(&old, &[3, 4, 5, 6, 7, 8, 9, 10]), Some(2));
        // Bottom rows may differ (partial line, prompt).
        assert_eq!(detect_shift(&old, &[2, 3, 4, 5, 6, 7, 99, 98]), Some(1));
        assert_eq!(detect_shift(&old, &[50, 51, 52, 53, 54, 55, 56, 57]), None);
    }

    fn snapshot(lines: &str, history_lines: usize) -> GridSnapshot {
        use maomi_term::alacritty_terminal::Term;
        use maomi_term::alacritty_terminal::event::VoidListener;
        use maomi_term::alacritty_terminal::term::Config;
        use maomi_term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
        let size = maomi_term::TermSize::new(10, 4);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for i in 0..history_lines + 4 {
            parser.advance(&mut term, format!("h{i}\r\n").as_bytes());
        }
        parser.advance(&mut term, lines.as_bytes());
        GridSnapshot::from_term(&term)
    }

    #[test]
    fn history_growth_starts_an_ease_that_settles() {
        let mut smooth = SmoothScroll::default();
        let t0 = Instant::now();
        assert_eq!(smooth.update(&snapshot("", 0), t0, true), 0.);
        let offset = smooth.update(&snapshot("", 2), t0, true);
        assert!((offset - 2.).abs() < 1e-6, "{offset}");
        let mid = smooth.update(&snapshot("", 2), t0 + Duration::from_millis(50), true);
        assert!(mid > 0. && mid < 2., "{mid}");
        let later = smooth.update(&snapshot("", 2), t0 + Duration::from_millis(400), true);
        assert_eq!(later, 0.);
    }

    #[test]
    fn velocity_carries_over_new_lines() {
        let mut smooth = SmoothScroll::default();
        let t0 = Instant::now();
        smooth.update(&snapshot("", 0), t0, true);
        smooth.update(&snapshot("", 1), t0, true);
        smooth.update(&snapshot("", 1), t0 + Duration::from_millis(30), true);
        let speed = smooth.velocity;
        assert!(speed < 0.);
        // A new line adds distance but keeps the speed it had.
        smooth.update(&snapshot("", 2), t0 + Duration::from_millis(30), true);
        assert_eq!(smooth.velocity, speed);
    }

    #[test]
    fn bursts_scroll_instantly() {
        let mut smooth = SmoothScroll::default();
        let t0 = Instant::now();
        smooth.update(&snapshot("", 0), t0, true);
        assert_eq!(
            smooth.update(&snapshot("", 500), t0, true),
            0.,
            "a big jump does not glide"
        );
        // Steady spam: two lines every 30 ms becomes a burst after a few frames.
        let mut smooth = SmoothScroll::default();
        smooth.update(&snapshot("", 0), t0, true);
        let mut last = 1.;
        for i in 1..=6 {
            last = smooth.update(
                &snapshot("", 2 * i),
                t0 + Duration::from_millis(30 * i as u64),
                true,
            );
        }
        assert_eq!(last, 0.);
    }

    #[test]
    fn glides_are_capped() {
        let mut smooth = SmoothScroll::default();
        let t0 = Instant::now();
        smooth.update(&snapshot("", 0), t0, true);
        assert_eq!(smooth.update(&snapshot("", 2), t0, true), 2.);
        assert_eq!(smooth.update(&snapshot("", 4), t0, true), MAX_GLIDE);
    }

    #[test]
    fn disabled_never_offsets() {
        let mut smooth = SmoothScroll::default();
        let t0 = Instant::now();
        smooth.update(&snapshot("", 0), t0, false);
        assert_eq!(smooth.update(&snapshot("", 3), t0, false), 0.);
    }
}
