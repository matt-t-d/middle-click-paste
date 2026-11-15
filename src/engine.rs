//! The select-buffer logic: what to do on a selection gesture and on a middle-click.
//!
//! Everything that touches the OS goes through [`Platform`], so the logic here can be
//! tested against a fake.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ax::{Selection, Target};
use crate::input::{Event, Gesture, Verdict};

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Paste on every middle-click, not only over editable text.
    pub always: bool,
    /// Paste only where Accessibility positively reports editable text. Without this,
    /// windows that are opaque to Accessibility (GPU terminals, ...) also get pastes.
    pub strict: bool,
    /// When Accessibility can't report the selection, borrow the clipboard with a
    /// synthetic Cmd+C (and put the clipboard back afterwards).
    pub copy_fallback: bool,
    pub verbose: bool,
}

/// OS operations the engine needs.
pub trait Platform {
    type Snapshot;
    fn selected_text(&self) -> Selection;
    fn change_count(&self) -> i64;
    fn read_string(&self) -> Option<String>;
    fn snapshot(&self) -> Self::Snapshot;
    fn restore(&self, snap: &Self::Snapshot);
    fn write_transient_string(&self, text: &str);
    fn post_copy(&self);
    fn post_paste(&self);
    fn post_left_click(&self, x: f64, y: f64);
    fn sleep(&self, d: Duration);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Job {
    Capture(Gesture),
    Paste { x: f64, y: f64 },
}

/// The second clipboard. Shared between the tap thread (reads emptiness) and the worker.
pub type Buffer = Arc<Mutex<String>>;

// Time for the app to finish updating its selection after the gesture ends.
const SETTLE: Duration = Duration::from_millis(60);
// Time for the app to move focus/caret after our synthetic left click.
const CLICK_SETTLE: Duration = Duration::from_millis(60);
// How long to wait for a synthetic Cmd+C to land on the clipboard.
const COPY_TIMEOUT: Duration = Duration::from_millis(300);
const COPY_POLL: Duration = Duration::from_millis(10);
// Time for the app to read the clipboard after our synthetic Cmd+V, before we restore it.
const PASTE_SETTLE: Duration = Duration::from_millis(250);

/// Decide what to do with a tap event. Runs on the tap thread, so it must be quick:
/// real work is queued for the worker.
pub fn on_event(
    event: Event,
    cfg: &Config,
    buffer: &Buffer,
    jobs: &Sender<Job>,
    target_at: impl Fn(f64, f64) -> Target,
) -> Verdict {
    match event {
        Event::Selection(gesture) => {
            let _ = jobs.send(Job::Capture(gesture));
            Verdict::Pass
        }
        Event::MiddleClick { x, y } => {
            if buffer.lock().unwrap().is_empty() {
                return Verdict::Pass;
            }
            if !cfg.always {
                match target_at(x, y) {
                    Target::Text => {}
                    Target::Unknown if !cfg.strict => {}
                    _ => return Verdict::Pass,
                }
            }
            match jobs.send(Job::Paste { x, y }) {
                Ok(()) => Verdict::Swallow,
                Err(_) => Verdict::Pass,
            }
        }
    }
}

/// Worker loop: runs until the sending side is dropped.
pub fn run_worker<P: Platform>(platform: &P, cfg: &Config, buffer: &Buffer, jobs: Receiver<Job>) {
    let mut pending: VecDeque<Job> = VecDeque::new();
    loop {
        let job = match pending.pop_front() {
            Some(job) => job,
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        match job {
            Job::Capture(mut gesture) => {
                platform.sleep(SETTLE);
                // A double-click followed by a triple-click (or key repeats) queues several
                // captures; only the last one matters.
                pending.extend(jobs.try_iter());
                while let Some(Job::Capture(next)) = pending.front().copied() {
                    gesture = next;
                    pending.pop_front();
                }
                capture(platform, cfg, buffer, gesture);
            }
            Job::Paste { x, y } => paste(platform, cfg, buffer, x, y),
        }
    }
}

fn capture<P: Platform>(platform: &P, cfg: &Config, buffer: &Buffer, gesture: Gesture) {
    let text = match platform.selected_text() {
        Selection::Text(text) => Some(text),
        // Like X11/Wayland primary selection in practice: deselecting keeps the old buffer.
        Selection::Empty => None,
        Selection::Unsupported => {
            // Keyboard gestures are too ambiguous (Cmd+A in Finder, shift+arrows in a
            // game...) to justify sending keystrokes.
            if cfg.copy_fallback && gesture != Gesture::Keyboard {
                copy_via_clipboard(platform)
            } else {
                None
            }
        }
    };
    match text {
        Some(text) if !text.is_empty() => {
            if cfg.verbose {
                eprintln!("captured {} chars via {:?}", text.chars().count(), gesture);
            }
            *buffer.lock().unwrap() = text;
        }
        _ => {
            if cfg.verbose {
                eprintln!("no selection after {:?}", gesture);
            }
        }
    }
}

/// Send Cmd+C, read what lands on the clipboard, then put the clipboard back.
fn copy_via_clipboard<P: Platform>(platform: &P) -> Option<String> {
    let saved = platform.snapshot();
    let before = platform.change_count();
    platform.post_copy();

    let mut waited = Duration::ZERO;
    while platform.change_count() == before {
        if waited >= COPY_TIMEOUT {
            // Nothing was copied, so the clipboard is untouched: nothing to restore.
            return None;
        }
        platform.sleep(COPY_POLL);
        waited += COPY_POLL;
    }
    let text = platform.read_string();
    platform.restore(&saved);
    text
}

fn paste<P: Platform>(platform: &P, cfg: &Config, buffer: &Buffer, x: f64, y: f64) {
    let text = buffer.lock().unwrap().clone();
    if text.is_empty() {
        return;
    }
    if cfg.verbose {
        eprintln!("pasting {} chars at ({x:.0}, {y:.0})", text.chars().count());
    }
    // Put the caret where the pointer is, as a middle-click does on Linux.
    platform.post_left_click(x, y);
    platform.sleep(CLICK_SETTLE);

    let saved = platform.snapshot();
    platform.write_transient_string(&text);
    platform.post_paste();
    platform.sleep(PASTE_SETTLE);
    platform.restore(&saved);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::mpsc::channel;

    /// Fake OS: a clipboard, a "selection" that Cmd+C copies, and a log of what happened.
    struct Fake {
        ax: Selection,
        /// What a Cmd+C would put on the clipboard (None: app ignores Cmd+C).
        copyable: Option<String>,
        clipboard: RefCell<String>,
        changes: RefCell<i64>,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(ax: Selection, copyable: Option<&str>) -> Self {
            Fake {
                ax,
                copyable: copyable.map(String::from),
                clipboard: RefCell::new("user clipboard".into()),
                changes: RefCell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }
        fn set_clipboard(&self, s: &str) {
            *self.clipboard.borrow_mut() = s.into();
            *self.changes.borrow_mut() += 1;
        }
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Platform for Fake {
        type Snapshot = String;
        fn selected_text(&self) -> Selection {
            self.ax.clone()
        }
        fn change_count(&self) -> i64 {
            *self.changes.borrow()
        }
        fn read_string(&self) -> Option<String> {
            Some(self.clipboard.borrow().clone())
        }
        fn snapshot(&self) -> String {
            self.clipboard.borrow().clone()
        }
        fn restore(&self, snap: &String) {
            self.set_clipboard(snap);
        }
        fn write_transient_string(&self, text: &str) {
            self.set_clipboard(text);
        }
        fn post_copy(&self) {
            self.log.borrow_mut().push("copy".into());
            if let Some(text) = &self.copyable {
                self.set_clipboard(text);
            }
        }
        fn post_paste(&self) {
            let pasted = self.clipboard.borrow().clone();
            self.log.borrow_mut().push(format!("paste:{pasted}"));
        }
        fn post_left_click(&self, x: f64, y: f64) {
            self.log.borrow_mut().push(format!("click:{x},{y}"));
        }
        fn sleep(&self, _d: Duration) {}
    }

    const CFG: Config =
        Config { always: false, strict: false, copy_fallback: true, verbose: false };

    fn run(fake: &Fake, cfg: &Config, buffer: &Buffer, jobs: &[Job]) {
        let (tx, rx) = channel();
        for job in jobs {
            tx.send(*job).unwrap();
        }
        drop(tx);
        run_worker(fake, cfg, buffer, rx);
    }

    fn buffer_with(s: &str) -> Buffer {
        Arc::new(Mutex::new(s.to_string()))
    }

    #[test]
    fn ax_selection_fills_buffer_without_touching_clipboard() {
        let fake = Fake::new(Selection::Text("hello".into()), Some("should not be used"));
        let buffer = buffer_with("");
        run(&fake, &CFG, &buffer, &[Job::Capture(Gesture::Drag)]);
        assert_eq!(*buffer.lock().unwrap(), "hello");
        assert_eq!(*fake.clipboard.borrow(), "user clipboard");
        assert_eq!(fake.change_count(), 0);
        assert!(fake.log().is_empty());
    }

    #[test]
    fn empty_selection_keeps_previous_buffer() {
        let fake = Fake::new(Selection::Empty, Some("x"));
        let buffer = buffer_with("old");
        run(&fake, &CFG, &buffer, &[Job::Capture(Gesture::Drag)]);
        assert_eq!(*buffer.lock().unwrap(), "old");
        assert!(fake.log().is_empty(), "no Cmd+C when AX works");
    }

    #[test]
    fn fallback_copies_then_restores_clipboard() {
        let fake = Fake::new(Selection::Unsupported, Some("from cmd-c"));
        let buffer = buffer_with("");
        run(&fake, &CFG, &buffer, &[Job::Capture(Gesture::MultiClick)]);
        assert_eq!(*buffer.lock().unwrap(), "from cmd-c");
        assert_eq!(*fake.clipboard.borrow(), "user clipboard");
        assert_eq!(fake.log(), ["copy"]);
    }

    #[test]
    fn fallback_with_nothing_copied_leaves_everything_alone() {
        let fake = Fake::new(Selection::Unsupported, None);
        let buffer = buffer_with("old");
        run(&fake, &CFG, &buffer, &[Job::Capture(Gesture::Drag)]);
        assert_eq!(*buffer.lock().unwrap(), "old");
        assert_eq!(fake.change_count(), 0, "clipboard never written");
    }

    #[test]
    fn fallback_skipped_for_keyboard_and_when_disabled() {
        let fake = Fake::new(Selection::Unsupported, Some("x"));
        let buffer = buffer_with("");
        run(&fake, &CFG, &buffer, &[Job::Capture(Gesture::Keyboard)]);
        let off = Config { copy_fallback: false, ..CFG };
        run(&fake, &off, &buffer, &[Job::Capture(Gesture::Drag)]);
        assert!(fake.log().is_empty());
        assert_eq!(*buffer.lock().unwrap(), "");
    }

    #[test]
    fn paste_clicks_pastes_buffer_and_restores_clipboard() {
        let fake = Fake::new(Selection::Empty, None);
        let buffer = buffer_with("primary");
        run(&fake, &CFG, &buffer, &[Job::Paste { x: 3.0, y: 4.0 }]);
        assert_eq!(fake.log(), ["click:3,4", "paste:primary"]);
        assert_eq!(*fake.clipboard.borrow(), "user clipboard");
        assert_eq!(*buffer.lock().unwrap(), "primary");
    }

    #[test]
    fn queued_captures_collapse_but_paste_still_runs() {
        let fake = Fake::new(Selection::Unsupported, Some("sel"));
        let buffer = buffer_with("");
        run(
            &fake,
            &CFG,
            &buffer,
            &[
                Job::Capture(Gesture::MultiClick),
                Job::Capture(Gesture::MultiClick),
                Job::Paste { x: 1.0, y: 2.0 },
            ],
        );
        assert_eq!(fake.log(), ["copy", "click:1,2", "paste:sel"]);
    }

    fn decide(event: Event, cfg: &Config, buffer: &Buffer, target: Target) -> (Verdict, Vec<Job>) {
        let (tx, rx) = channel();
        let verdict = on_event(event, cfg, buffer, &tx, |_, _| target);
        (verdict, rx.try_iter().collect())
    }

    #[test]
    fn middle_click_only_swallowed_over_text_with_a_buffer() {
        let click = Event::MiddleClick { x: 1.0, y: 2.0 };
        let paste = vec![Job::Paste { x: 1.0, y: 2.0 }];
        let full = buffer_with("x");

        assert_eq!(decide(click, &CFG, &full, Target::Text), (Verdict::Swallow, paste.clone()));
        assert_eq!(decide(click, &CFG, &full, Target::NotText), (Verdict::Pass, vec![]));
        assert_eq!(decide(click, &CFG, &full, Target::Unknown), (Verdict::Swallow, paste.clone()));
        let strict = Config { strict: true, ..CFG };
        assert_eq!(decide(click, &strict, &full, Target::Unknown), (Verdict::Pass, vec![]));
        assert_eq!(decide(click, &strict, &full, Target::Text), (Verdict::Swallow, paste.clone()));
        assert_eq!(decide(click, &CFG, &buffer_with(""), Target::Text), (Verdict::Pass, vec![]));

        let always = Config { always: true, ..CFG };
        assert_eq!(decide(click, &always, &full, Target::NotText), (Verdict::Swallow, paste));
        assert_eq!(decide(click, &always, &buffer_with(""), Target::Text), (Verdict::Pass, vec![]));
    }

    #[test]
    fn selection_event_queues_capture_and_passes() {
        let (verdict, jobs) =
            decide(Event::Selection(Gesture::Drag), &CFG, &buffer_with(""), Target::Unknown);
        assert_eq!(verdict, Verdict::Pass);
        assert_eq!(jobs, vec![Job::Capture(Gesture::Drag)]);
    }
}
