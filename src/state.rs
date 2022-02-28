use std::borrow::Cow;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::draw_target::ProgressDrawTarget;
use crate::style::ProgressStyle;

pub(crate) struct BarState {
    pub(crate) draw_target: ProgressDrawTarget,
    pub(crate) on_finish: ProgressFinish,
    pub(crate) style: ProgressStyle,
    pub(crate) state: ProgressState,
    pub(crate) ticker: Option<(Duration, thread::JoinHandle<()>)>,
}

impl BarState {
    pub(crate) fn new(len: u64, draw_target: ProgressDrawTarget) -> Self {
        Self {
            draw_target,
            on_finish: ProgressFinish::default(),
            style: ProgressStyle::default_bar(),
            state: ProgressState::new(len),
            ticker: None,
        }
    }

    /// Finishes the progress bar using the [`ProgressFinish`] behavior stored
    /// in the [`ProgressStyle`].
    pub(crate) fn finish_using_style(&mut self, now: Instant, finish: ProgressFinish) {
        self.state.status = Status::DoneVisible;
        match finish {
            ProgressFinish::AndLeave => self.state.pos = self.state.len,
            ProgressFinish::WithMessage(msg) => {
                self.state.pos = self.state.len;
                self.style.message = msg;
            }
            ProgressFinish::AndClear => {
                self.state.pos = self.state.len;
                self.state.status = Status::DoneHidden;
            }
            ProgressFinish::Abandon => {}
            ProgressFinish::AbandonWithMessage(msg) => self.style.message = msg,
        }

        // There's no need to update the estimate here; once the `status` is no longer
        // `InProgress`, we will use the length and elapsed time to estimate.
        let _ = self.draw(true, now);
    }

    pub(crate) fn reset(&mut self, now: Instant, mode: Reset) {
        if let Reset::Eta | Reset::All = mode {
            self.state.est.reset(now);
        }

        if let Reset::Elapsed | Reset::All = mode {
            self.state.started = now;
        }

        if let Reset::All = mode {
            self.state.pos = 0;
            self.state.status = Status::InProgress;
            let _ = self.draw(false, now);
        }
    }

    pub(crate) fn update(&mut self, now: Instant, f: impl FnOnce(&mut ProgressState)) {
        let old = self.state.pos;
        f(&mut self.state);
        let delta = self.state.pos.saturating_sub(old);
        self.state.est.record(delta, now);
        self.tick(now);
    }

    pub(crate) fn set_position(&mut self, now: Instant, new: u64) {
        let old = self.state.pos;
        self.state.pos = new;
        self.state.est.record(new.saturating_sub(old), now);
        self.tick(now);
    }

    pub(crate) fn inc(&mut self, now: Instant, delta: u64) {
        self.state.pos = self.state.pos.saturating_add(delta);
        self.state.est.record(delta, now);
        self.tick(now);
    }

    pub(crate) fn set_length(&mut self, now: Instant, len: u64) {
        self.state.len = len;
        self.tick(now);
    }

    pub(crate) fn inc_length(&mut self, now: Instant, delta: u64) {
        self.state.len = self.state.len.saturating_add(delta);
        self.tick(now);
    }

    pub(crate) fn set_message(&mut self, now: Instant, msg: Cow<'static, str>) {
        self.style.message = msg;
        self.tick(now);
    }

    pub(crate) fn set_prefix(&mut self, now: Instant, prefix: Cow<'static, str>) {
        self.style.prefix = prefix;
        self.tick(now);
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        if self.ticker.is_none() || self.state.tick == 0 {
            self.state.tick = self.state.tick.saturating_add(1);
        }

        let _ = self.draw(false, now);
    }

    pub(crate) fn println(&mut self, now: Instant, msg: &str) {
        let width = self.draw_target.width();
        let mut drawable = match self.draw_target.drawable(true, now) {
            Some(drawable) => drawable,
            None => return,
        };

        let mut draw_state = drawable.state();
        draw_state.lines.extend(msg.lines().map(Into::into));
        draw_state.orphan_lines = draw_state.lines.len();
        if !matches!(self.state.status, Status::DoneHidden) {
            self.style
                .format_state(&self.state, &mut draw_state.lines, width);
        }

        drop(draw_state);
        let _ = drawable.draw();
    }

    pub(crate) fn suspend<F: FnOnce() -> R, R>(&mut self, now: Instant, f: F) -> R {
        if let Some(drawable) = self.draw_target.drawable(true, now) {
            let _ = drawable.clear();
        }

        let ret = f();
        let _ = self.draw(true, now);
        ret
    }

    fn draw(&mut self, mut force_draw: bool, now: Instant) -> io::Result<()> {
        let width = self.draw_target.width();
        force_draw |= self.state.is_finished();
        let mut drawable = match self.draw_target.drawable(force_draw, now) {
            Some(drawable) => drawable,
            None => return Ok(()),
        };

        // `|| self.is_finished()` should not be needed here, but we used to always for draw for
        // finished progress bar, so it's kept as to not cause compatibility issues in weird cases.
        let mut draw_state = drawable.state();

        if !matches!(self.state.status, Status::DoneHidden) {
            self.style
                .format_state(&self.state, &mut draw_state.lines, width);
        }

        drop(draw_state);
        drawable.draw()
    }
}

impl Drop for BarState {
    fn drop(&mut self) {
        // Progress bar is already finished.  Do not need to do anything.
        if self.state.is_finished() {
            return;
        }

        self.finish_using_style(Instant::now(), self.on_finish.clone());
    }
}

pub(crate) enum Reset {
    Eta,
    Elapsed,
    All,
}

/// The state of a progress bar at a moment in time.
#[non_exhaustive]
pub struct ProgressState {
    pos: u64,
    len: u64,
    pub(crate) tick: u64,
    pub(crate) started: Instant,
    status: Status,
    est: Estimator,
}

impl ProgressState {
    pub(crate) fn new(len: u64) -> Self {
        Self {
            pos: 0,
            len,
            tick: 0,
            status: Status::InProgress,
            started: Instant::now(),
            est: Estimator::new(Instant::now()),
        }
    }

    /// Indicates that the progress bar finished.
    pub fn is_finished(&self) -> bool {
        match self.status {
            Status::InProgress => false,
            Status::DoneVisible => true,
            Status::DoneHidden => true,
        }
    }

    /// Returns the completion as a floating-point number between 0 and 1
    pub fn fraction(&self) -> f32 {
        let pct = match (self.pos, self.len) {
            (_, 0) => 1.0,
            (0, _) => 0.0,
            (pos, len) => pos as f32 / len as f32,
        };
        pct.max(0.0).min(1.0)
    }

    /// The expected ETA
    pub fn eta(&self) -> Duration {
        if self.len == !0 || self.is_finished() {
            return Duration::new(0, 0);
        }
        let t = self.est.seconds_per_step();
        secs_to_duration(t * self.len.saturating_sub(self.pos) as f64)
    }

    /// The expected total duration (that is, elapsed time + expected ETA)
    pub fn duration(&self) -> Duration {
        if self.len == !0 || self.is_finished() {
            return Duration::new(0, 0);
        }
        self.started.elapsed() + self.eta()
    }

    /// The number of steps per second
    pub fn per_sec(&self) -> f64 {
        match self.status {
            Status::InProgress => match 1.0 / self.est.seconds_per_step() {
                per_sec if per_sec.is_nan() => 0.0,
                per_sec => per_sec,
            },
            _ => self.len as f64 / self.started.elapsed().as_secs_f64(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn set_pos(&mut self, pos: u64) {
        self.pos = pos;
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn set_len(&mut self, len: u64) {
        self.len = len;
    }
}

pub(crate) struct Ticker {
    weak: Weak<Mutex<BarState>>,
    interval: Duration,
}

impl Ticker {
    pub(crate) fn spawn(arc: &Arc<Mutex<BarState>>, interval: Duration) {
        let mut state = arc.lock().unwrap();
        if interval.is_zero() {
            return;
        } else if let Some((old, _)) = &mut state.ticker {
            *old = interval;
            return;
        }

        let ticker = Self {
            // Using a weak pointer is required to prevent a potential deadlock. See issue #133
            weak: Arc::downgrade(arc),
            interval,
        };

        let handle = thread::spawn(move || ticker.run());
        state.ticker = Some((interval, handle));
        drop(state);
        // use the side effect of tick to force the bar to tick.
        arc.lock().unwrap().tick(Instant::now());
    }

    fn run(mut self) {
        thread::sleep(self.interval);
        while let Some(arc) = self.weak.upgrade() {
            let mut state = arc.lock().unwrap();
            let interval = match state.ticker {
                Some((interval, _)) if !state.state.is_finished() => interval,
                _ => return,
            };

            if state.state.tick != 0 {
                state.state.tick = state.state.tick.saturating_add(1);
            }

            self.interval = interval;
            state.draw(false, Instant::now()).ok();
            thread::sleep(self.interval);
        }
    }
}

/// Estimate the number of seconds per step
///
/// Ring buffer with constant capacity. Used by `ProgressBar`s to display `{eta}`,
/// `{eta_precise}`, and `{*_per_sec}`.
pub(crate) struct Estimator {
    steps: [f64; 16],
    pos: u8,
    full: bool,
    prev: Instant,
}

impl Estimator {
    fn new(now: Instant) -> Self {
        Self {
            steps: [0.0; 16],
            pos: 0,
            full: false,
            prev: now,
        }
    }

    fn record(&mut self, delta: u64, now: Instant) {
        if delta == 0 {
            return;
        }

        let elapsed = now - self.prev;
        let divisor = delta as f64;
        let mut batch = 0.0;
        if divisor != 0.0 {
            batch = duration_to_secs(elapsed) / divisor;
        };

        self.steps[self.pos as usize] = batch;
        self.pos = (self.pos + 1) % 16;
        if !self.full && self.pos == 0 {
            self.full = true;
        }

        self.prev = now;
    }

    pub(crate) fn reset(&mut self, now: Instant) {
        self.pos = 0;
        self.full = false;
        self.prev = now;
    }

    /// Average time per step in seconds, using rolling buffer of last 15 steps
    fn seconds_per_step(&self) -> f64 {
        let len = self.len();
        self.steps[0..len].iter().sum::<f64>() / len as f64
    }

    fn len(&self) -> usize {
        match self.full {
            true => 16,
            false => self.pos as usize,
        }
    }
}

impl fmt::Debug for Estimator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Estimate")
            .field("steps", &&self.steps[..self.len()])
            .field("prev", &self.prev)
            .finish()
    }
}

/// Behavior of a progress bar when it is finished
///
/// This is invoked when a [`ProgressBar`] or [`ProgressBarIter`] completes and
/// [`ProgressBar::is_finished`] is false.
///
/// [`ProgressBar`]: crate::ProgressBar
/// [`ProgressBarIter`]: crate::ProgressBarIter
/// [`ProgressBar::is_finished`]: crate::ProgressBar::is_finished
#[derive(Clone, Debug)]
pub enum ProgressFinish {
    /// Finishes the progress bar and leaves the current message
    ///
    /// Same behavior as calling [`ProgressBar::finish()`](crate::ProgressBar::finish).
    AndLeave,
    /// Finishes the progress bar and sets a message
    ///
    /// Same behavior as calling [`ProgressBar::finish_with_message()`](crate::ProgressBar::finish_with_message).
    WithMessage(Cow<'static, str>),
    /// Finishes the progress bar and completely clears it (this is the default)
    ///
    /// Same behavior as calling [`ProgressBar::finish_and_clear()`](crate::ProgressBar::finish_and_clear).
    AndClear,
    /// Finishes the progress bar and leaves the current message and progress
    ///
    /// Same behavior as calling [`ProgressBar::abandon()`](crate::ProgressBar::abandon).
    Abandon,
    /// Finishes the progress bar and sets a message, and leaves the current progress
    ///
    /// Same behavior as calling [`ProgressBar::abandon_with_message()`](crate::ProgressBar::abandon_with_message).
    AbandonWithMessage(Cow<'static, str>),
}

impl Default for ProgressFinish {
    fn default() -> Self {
        Self::AndClear
    }
}

fn duration_to_secs(d: Duration) -> f64 {
    d.as_secs() as f64 + f64::from(d.subsec_nanos()) / 1_000_000_000f64
}

fn secs_to_duration(s: f64) -> Duration {
    let secs = s.trunc() as u64;
    let nanos = (s.fract() * 1_000_000_000f64) as u32;
    Duration::new(secs, nanos)
}

#[derive(Debug)]
pub(crate) enum Status {
    InProgress,
    DoneVisible,
    DoneHidden,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_per_step() {
        let test_rate = |items_per_second| {
            let mut est = Estimator::new(Instant::now());
            let mut current_time = est.prev;
            for _ in 0..est.steps.len() {
                current_time += Duration::from_secs(1);
                est.record(items_per_second, current_time);
            }
            let avg_seconds_per_step = est.seconds_per_step();

            assert!(avg_seconds_per_step > 0.0);
            assert!(avg_seconds_per_step.is_finite());

            let expected_rate = 1.0 / items_per_second as f64;
            let absolute_error = (avg_seconds_per_step - expected_rate).abs();
            assert!(
                absolute_error < f64::EPSILON,
                "Expected rate: {}, actual: {}, absolute error: {}",
                expected_rate,
                avg_seconds_per_step,
                absolute_error
            );
        };

        test_rate(1);
        test_rate(1_000);
        test_rate(1_000_000);
        test_rate(1_000_000_000);
        test_rate(1_000_000_001);
        test_rate(100_000_000_000);
        test_rate(1_000_000_000_000);
        test_rate(100_000_000_000_000);
        test_rate(1_000_000_000_000_000);
    }

    #[test]
    fn test_duration_stuff() {
        let duration = Duration::new(42, 100_000_000);
        let secs = duration_to_secs(duration);
        assert_eq!(secs_to_duration(secs), duration);
    }
}
