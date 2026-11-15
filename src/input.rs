//! Global input: CGEventTap listener + synthetic event posting.
//! STUB: signatures are a fixed contract; implementation pending.

/// How the user (probably) just made a text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Left button pressed, moved beyond a small threshold, released.
    Drag,
    /// Double or triple click (word / line selection).
    MultiClick,
    /// Shift + left click (extend selection).
    ShiftClick,
    /// Keyboard selection: shift+navigation keys, or Cmd+A.
    Keyboard,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// Emitted when the gesture completes (mouse up / key up).
    Selection(Gesture),
    /// Middle (button 2) mouse down at global coordinates (top-left origin).
    MiddleClick { x: f64, y: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Let the event through to applications.
    Pass,
    /// Consume it. Only meaningful for `MiddleClick`; the matching mouse-up is consumed too.
    Swallow,
}

/// Install a session-level event tap on the current thread's run loop and run it forever.
/// `handler` is invoked on this thread for every `Event`; synthetic events posted by
/// this module are never reported. Returns `Err` only if the tap can't be created
/// (typically: missing Accessibility permission).
pub fn run_tap(_handler: Box<dyn FnMut(Event) -> Verdict>) -> Result<(), String> {
    todo!()
}

/// Post Cmd+C to the focused application. Callable from any thread.
pub fn post_copy() {
    todo!()
}

/// Post Cmd+V to the focused application. Callable from any thread.
pub fn post_paste() {
    todo!()
}

/// Post a left mouse click (down+up) at global coordinates. Callable from any thread.
pub fn post_left_click(_x: f64, _y: f64) {
    todo!()
}
