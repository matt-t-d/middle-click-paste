//! Global input: CGEventTap listener + synthetic event posting.
//!
//! Layout: a pure gesture state machine (`Tracker`, unit-tested below), the FFI glue
//! that feeds it from a CGEventTap, and the `post_*` helpers that synthesize input.

use crate::ffi::{CFAllocatorRef, CFIndex, CFRelease, CFStringRef, CFTypeRef, CGPoint};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU16, Ordering};

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

// ---------------------------------------------------------------------------
// Pure gesture state machine
// ---------------------------------------------------------------------------

/// Pointer displacement (in points) beyond which a press-release counts as a drag.
const DRAG_THRESHOLD: f64 = 4.0;

const MIDDLE_BUTTON: i64 = 2;

// Virtual keycodes (HIToolbox/Events.h).
const VK_ANSI_A: u16 = 0x00;
const VK_ANSI_C: u16 = 0x08;
const VK_ANSI_V: u16 = 0x09;
const VK_HOME: u16 = 0x73;
const VK_PAGE_UP: u16 = 0x74;
const VK_END: u16 = 0x77;
const VK_PAGE_DOWN: u16 = 0x79;
const VK_LEFT: u16 = 0x7B;
const VK_RIGHT: u16 = 0x7C;
const VK_DOWN: u16 = 0x7D;
const VK_UP: u16 = 0x7E;

fn is_nav_key(code: u16) -> bool {
    matches!(
        code,
        VK_HOME | VK_PAGE_UP | VK_END | VK_PAGE_DOWN | VK_LEFT | VK_RIGHT | VK_DOWN | VK_UP
    )
}

/// Does this key press (with these modifiers) change the selection?
fn is_selection_key(code: u16, shift: bool, cmd: bool) -> bool {
    (shift && is_nav_key(code)) || (cmd && !shift && code == VK_ANSI_A)
}

#[derive(Debug, Default)]
struct Tracker {
    /// Where the left button went down, if we saw it go down.
    left_down_at: Option<(f64, f64)>,
    /// Pointer moved beyond the threshold since the left button went down.
    dragged: bool,
    /// Keycodes (< 128) that went down as a selection key and haven't come up yet.
    /// Recorded at key-down so that releasing Shift/Cmd before the key still counts.
    pending_keys: u128,
    /// The last button-2 down was swallowed, so its up must be swallowed too.
    swallow_middle_up: bool,
}

impl Tracker {
    fn note_position(&mut self, x: f64, y: f64) {
        if let Some((x0, y0)) = self.left_down_at {
            let (dx, dy) = (x - x0, y - y0);
            if dx * dx + dy * dy > DRAG_THRESHOLD * DRAG_THRESHOLD {
                self.dragged = true;
            }
        }
    }

    fn left_down(&mut self, x: f64, y: f64) {
        self.left_down_at = Some((x, y));
        self.dragged = false;
    }

    fn left_dragged(&mut self, x: f64, y: f64) {
        self.note_position(x, y);
    }

    fn left_up(&mut self, x: f64, y: f64, click_state: i64, shift: bool) -> Option<Gesture> {
        self.note_position(x, y);
        let dragged = self.dragged;
        self.left_down_at = None;
        self.dragged = false;
        if dragged {
            Some(Gesture::Drag)
        } else if click_state >= 2 {
            Some(Gesture::MultiClick)
        } else if shift {
            Some(Gesture::ShiftClick)
        } else {
            None
        }
    }

    fn key_down(&mut self, code: u16, shift: bool, cmd: bool) {
        if code < 128 && is_selection_key(code, shift, cmd) {
            self.pending_keys |= 1u128 << code;
        }
    }

    fn key_up(&mut self, code: u16, shift: bool, cmd: bool) -> Option<Gesture> {
        let pending = code < 128 && self.pending_keys & (1u128 << code) != 0;
        if code < 128 {
            self.pending_keys &= !(1u128 << code);
        }
        if pending || is_selection_key(code, shift, cmd) {
            Some(Gesture::Keyboard)
        } else {
            None
        }
    }

    /// Returns the event to report, if any. The caller must follow up with
    /// `middle_verdict` using the handler's answer.
    fn other_down(&mut self, button: i64, x: f64, y: f64) -> Option<Event> {
        if button != MIDDLE_BUTTON {
            return None;
        }
        self.swallow_middle_up = false;
        Some(Event::MiddleClick { x, y })
    }

    fn middle_verdict(&mut self, verdict: Verdict) {
        self.swallow_middle_up = verdict == Verdict::Swallow;
    }

    /// Button-2 drag between a swallowed down and its up: swallow as well, so
    /// applications never see a drag without a down.
    fn other_dragged(&self, button: i64) -> Verdict {
        if button == MIDDLE_BUTTON && self.swallow_middle_up {
            Verdict::Swallow
        } else {
            Verdict::Pass
        }
    }

    fn other_up(&mut self, button: i64) -> Verdict {
        if button == MIDDLE_BUTTON && self.swallow_middle_up {
            self.swallow_middle_up = false;
            Verdict::Swallow
        } else {
            Verdict::Pass
        }
    }
}

/// Pick the virtual keycode that produces `want` under the current layout, preferring
/// `preferred` (the ANSI position) and falling back to it when nothing matches.
fn pick_keycode(translate: impl Fn(u16) -> Option<char>, want: char, preferred: u16) -> u16 {
    if translate(preferred) == Some(want) {
        return preferred;
    }
    (0u16..128)
        .find(|&code| translate(code) == Some(want))
        .unwrap_or(preferred)
}

// ---------------------------------------------------------------------------
// FFI
// ---------------------------------------------------------------------------

type CGEventRef = *mut c_void;
type CGEventSourceRef = *const c_void;
type CGEventTapProxy = *const c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFNotificationCenterRef = *const c_void;
type CFDataRef = *const c_void;
type TISInputSourceRef = *const c_void;
type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef;
type CFNotificationCallback = unsafe extern "C" fn(
    center: CFNotificationCenterRef,
    observer: *mut c_void,
    name: CFStringRef,
    object: *const c_void,
    user_info: *const c_void,
);

// CGEventTypes.h / IOLLEvent.h
const EV_LEFT_MOUSE_DOWN: u32 = 1;
const EV_LEFT_MOUSE_UP: u32 = 2;
const EV_LEFT_MOUSE_DRAGGED: u32 = 6;
const EV_KEY_DOWN: u32 = 10;
const EV_KEY_UP: u32 = 11;
const EV_OTHER_MOUSE_DOWN: u32 = 25;
const EV_OTHER_MOUSE_UP: u32 = 26;
const EV_OTHER_MOUSE_DRAGGED: u32 = 27;
const EV_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const EV_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

const FIELD_MOUSE_CLICK_STATE: u32 = 1;
const FIELD_MOUSE_BUTTON_NUMBER: u32 = 3;
const FIELD_KEYBOARD_KEYCODE: u32 = 9;
const FIELD_SOURCE_USER_DATA: u32 = 42;

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

const TAP_LOCATION_HID: u32 = 0;
const TAP_LOCATION_SESSION: u32 = 1;
const TAP_PLACEMENT_HEAD_INSERT: u32 = 0;
const TAP_OPTION_DEFAULT: u32 = 0;
const MOUSE_BUTTON_LEFT: u32 = 0;

/// CFNotificationSuspensionBehaviorDeliverImmediately
const DELIVER_IMMEDIATELY: CFIndex = 4;

/// Marks events posted by this module ("MCPaste" in ASCII); the tap ignores them.
const SYNTHETIC_MAGIC: i64 = 0x4D43_5061_7374_65;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        refcon: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
    fn CFNotificationCenterAddObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
        callback: CFNotificationCallback,
        name: CFStringRef,
        object: *const c_void,
        suspension_behavior: CFIndex,
    );
}

// Text Input Sources / Unicode Utilities, for layout-correct C and V keycodes.
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    static kTISNotifySelectedKeyboardInputSourceChanged: CFStringRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *const c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        key_layout: *const u8,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

extern "C" {
    fn pthread_main_np() -> i32;
}

// ---------------------------------------------------------------------------
// Layout-aware keycodes for Cmd+C / Cmd+V
// ---------------------------------------------------------------------------

static COPY_KEYCODE: AtomicU16 = AtomicU16::new(VK_ANSI_C);
static PASTE_KEYCODE: AtomicU16 = AtomicU16::new(VK_ANSI_V);

/// Find which keys produce "c" and "v" with Command held under the current keyboard
/// layout (so Dvorak works, "Dvorak - QWERTY ⌘" keeps the QWERTY positions, and
/// non-Latin layouts resolve through their Latin command mapping) and cache them for
/// `post_copy`/`post_paste`. Falls back to the ANSI positions.
///
/// The Text Input Sources API is main-thread-only, so this is a no-op elsewhere.
fn refresh_layout_keycodes() {
    unsafe {
        if pthread_main_np() == 0 {
            return;
        }
        let source = TISCopyCurrentKeyboardLayoutInputSource();
        if source.is_null() {
            return;
        }
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        if !data.is_null() {
            let layout = CFDataGetBytePtr(data);
            let keyboard_type = LMGetKbdType() as u32;
            let translate = |code: u16| -> Option<char> {
                const ACTION_DOWN: u16 = 0; // kUCKeyActionDown
                const MOD_COMMAND: u32 = 0x0100 >> 8; // (cmdKey >> 8) & 0xFF
                const NO_DEAD_KEYS: u32 = 1; // kUCKeyTranslateNoDeadKeysMask
                let mut dead_key_state = 0u32;
                let mut len = 0usize;
                let mut buf = [0u16; 4];
                let status = UCKeyTranslate(
                    layout,
                    code,
                    ACTION_DOWN,
                    MOD_COMMAND,
                    keyboard_type,
                    NO_DEAD_KEYS,
                    &mut dead_key_state,
                    buf.len(),
                    &mut len,
                    buf.as_mut_ptr(),
                );
                if status == 0 && len == 1 {
                    char::from_u32(buf[0] as u32)
                } else {
                    None
                }
            };
            if !layout.is_null() {
                COPY_KEYCODE.store(pick_keycode(&translate, 'c', VK_ANSI_C), Ordering::Relaxed);
                PASTE_KEYCODE.store(pick_keycode(&translate, 'v', VK_ANSI_V), Ordering::Relaxed);
            }
        }
        CFRelease(source as CFTypeRef);
    }
}

unsafe extern "C" fn layout_changed(
    _center: CFNotificationCenterRef,
    _observer: *mut c_void,
    _name: CFStringRef,
    _object: *const c_void,
    _user_info: *const c_void,
) {
    let _ = catch_unwind(refresh_layout_keycodes);
}

// ---------------------------------------------------------------------------
// Event tap
// ---------------------------------------------------------------------------

struct TapContext {
    handler: Box<dyn FnMut(Event) -> Verdict>,
    tracker: Tracker,
    tap: CFMachPortRef,
}

/// Returns true if the event should be swallowed.
unsafe fn process_event(ctx: &mut TapContext, etype: u32, event: CGEventRef) -> bool {
    if etype == EV_TAP_DISABLED_BY_TIMEOUT || etype == EV_TAP_DISABLED_BY_USER_INPUT {
        // We may have missed ups while disabled; start from a clean slate.
        ctx.tracker = Tracker::default();
        if !ctx.tap.is_null() {
            CGEventTapEnable(ctx.tap, true);
        }
        return false;
    }
    if event.is_null() || CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) == SYNTHETIC_MAGIC
    {
        return false;
    }

    let flags = CGEventGetFlags(event);
    let shift = flags & FLAG_SHIFT != 0;
    let cmd = flags & FLAG_COMMAND != 0;

    let mut gesture = None;
    let mut swallow = false;
    match etype {
        EV_LEFT_MOUSE_DOWN => {
            let p = CGEventGetLocation(event);
            ctx.tracker.left_down(p.x, p.y);
        }
        EV_LEFT_MOUSE_DRAGGED => {
            let p = CGEventGetLocation(event);
            ctx.tracker.left_dragged(p.x, p.y);
        }
        EV_LEFT_MOUSE_UP => {
            let p = CGEventGetLocation(event);
            let clicks = CGEventGetIntegerValueField(event, FIELD_MOUSE_CLICK_STATE);
            gesture = ctx.tracker.left_up(p.x, p.y, clicks, shift);
        }
        EV_KEY_DOWN => {
            let code = CGEventGetIntegerValueField(event, FIELD_KEYBOARD_KEYCODE) as u16;
            ctx.tracker.key_down(code, shift, cmd);
        }
        EV_KEY_UP => {
            let code = CGEventGetIntegerValueField(event, FIELD_KEYBOARD_KEYCODE) as u16;
            gesture = ctx.tracker.key_up(code, shift, cmd);
        }
        EV_OTHER_MOUSE_DOWN => {
            let button = CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON_NUMBER);
            let p = CGEventGetLocation(event);
            if let Some(ev) = ctx.tracker.other_down(button, p.x, p.y) {
                let verdict = (ctx.handler)(ev);
                ctx.tracker.middle_verdict(verdict);
                swallow = verdict == Verdict::Swallow;
            }
        }
        EV_OTHER_MOUSE_DRAGGED => {
            let button = CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON_NUMBER);
            swallow = ctx.tracker.other_dragged(button) == Verdict::Swallow;
        }
        EV_OTHER_MOUSE_UP => {
            let button = CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON_NUMBER);
            swallow = ctx.tracker.other_up(button) == Verdict::Swallow;
        }
        _ => {}
    }
    if let Some(g) = gesture {
        // Verdict is irrelevant for selections: always pass through.
        let _ = (ctx.handler)(Event::Selection(g));
    }
    swallow
}

unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef {
    let ctx = refcon as *mut TapContext;
    if ctx.is_null() {
        return event;
    }
    // A panic must not unwind into CoreGraphics; on panic, pass the event through.
    match catch_unwind(AssertUnwindSafe(|| process_event(&mut *ctx, etype, event))) {
        Ok(true) => std::ptr::null_mut(),
        Ok(false) => event,
        Err(_) => {
            (*ctx).tracker = Tracker::default();
            event
        }
    }
}

/// Install a session-level event tap on the current thread's run loop and run it forever.
/// `handler` is invoked on this thread for every `Event`; synthetic events posted by
/// this module are never reported. Returns `Err` only if the tap can't be created
/// (typically: missing Accessibility permission).
pub fn run_tap(handler: Box<dyn FnMut(Event) -> Verdict>) -> Result<(), String> {
    let mask: u64 = [
        EV_LEFT_MOUSE_DOWN,
        EV_LEFT_MOUSE_UP,
        EV_LEFT_MOUSE_DRAGGED,
        EV_KEY_DOWN,
        EV_KEY_UP,
        EV_OTHER_MOUSE_DOWN,
        EV_OTHER_MOUSE_UP,
        EV_OTHER_MOUSE_DRAGGED,
    ]
    .iter()
    .fold(0, |m, &t| m | (1u64 << t));

    let ctx = Box::into_raw(Box::new(TapContext {
        handler,
        tracker: Tracker::default(),
        tap: std::ptr::null_mut(),
    }));

    unsafe {
        let tap = CGEventTapCreate(
            TAP_LOCATION_SESSION,
            TAP_PLACEMENT_HEAD_INSERT,
            TAP_OPTION_DEFAULT,
            mask,
            tap_callback,
            ctx as *mut c_void,
        );
        if tap.is_null() {
            drop(Box::from_raw(ctx));
            return Err("CGEventTapCreate failed: grant this program Accessibility \
                 permission in System Settings > Privacy & Security > Accessibility"
                .to_string());
        }
        (*ctx).tap = tap;

        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        if source.is_null() {
            CFRelease(tap as CFTypeRef);
            drop(Box::from_raw(ctx));
            return Err("CFMachPortCreateRunLoopSource failed".to_string());
        }
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);

        // Keep the Cmd+C / Cmd+V keycodes in step with the keyboard layout.
        refresh_layout_keycodes();
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            std::ptr::null(),
            layout_changed,
            kTISNotifySelectedKeyboardInputSourceChanged,
            std::ptr::null(),
            DELIVER_IMMEDIATELY,
        );

        CFRunLoopRun();

        // Only reached if something stops this thread's run loop.
        CGEventTapEnable(tap, false);
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        CFRelease(source as CFTypeRef);
        CFRelease(tap as CFTypeRef);
        drop(Box::from_raw(ctx));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Synthetic input
// ---------------------------------------------------------------------------

unsafe fn post_tagged(event: CGEventRef, flags: u64) {
    if event.is_null() {
        return;
    }
    CGEventSetFlags(event, flags);
    CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, SYNTHETIC_MAGIC);
    CGEventPost(TAP_LOCATION_HID, event);
    CFRelease(event as CFTypeRef);
}

/// Key down + up with flags set to exactly Command, whatever is physically held.
fn post_command_key(keycode: u16) {
    unsafe {
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down);
            post_tagged(event, FLAG_COMMAND);
        }
    }
}

/// Post Cmd+C to the focused application. Callable from any thread.
pub fn post_copy() {
    post_command_key(COPY_KEYCODE.load(Ordering::Relaxed));
}

/// Post Cmd+V to the focused application. Callable from any thread.
pub fn post_paste() {
    post_command_key(PASTE_KEYCODE.load(Ordering::Relaxed));
}

/// Post a left mouse click (down+up) at global coordinates. Callable from any thread.
pub fn post_left_click(x: f64, y: f64) {
    unsafe {
        for etype in [EV_LEFT_MOUSE_DOWN, EV_LEFT_MOUSE_UP] {
            let event =
                CGEventCreateMouseEvent(std::ptr::null(), etype, CGPoint { x, y }, MOUSE_BUTTON_LEFT);
            if event.is_null() {
                continue;
            }
            CGEventSetIntegerValueField(event, FIELD_MOUSE_CLICK_STATE, 1);
            // No modifiers: a physically held Shift must not turn this into a shift-click.
            post_tagged(event, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_click_is_not_a_selection() {
        let mut t = Tracker::default();
        t.left_down(100.0, 100.0);
        assert_eq!(t.left_up(100.0, 100.0, 1, false), None);
    }

    #[test]
    fn jitter_below_threshold_is_not_a_drag() {
        let mut t = Tracker::default();
        t.left_down(100.0, 100.0);
        t.left_dragged(102.0, 101.0);
        assert_eq!(t.left_up(102.0, 102.0, 1, false), None);
    }

    #[test]
    fn movement_beyond_threshold_is_a_drag() {
        let mut t = Tracker::default();
        t.left_down(100.0, 100.0);
        t.left_dragged(130.0, 100.0);
        assert_eq!(t.left_up(160.0, 100.0, 1, false), Some(Gesture::Drag));
    }

    #[test]
    fn drag_that_returns_to_origin_still_counts() {
        let mut t = Tracker::default();
        t.left_down(100.0, 100.0);
        t.left_dragged(150.0, 100.0);
        t.left_dragged(100.0, 100.0);
        assert_eq!(t.left_up(100.0, 100.0, 1, false), Some(Gesture::Drag));
    }

    #[test]
    fn displacement_seen_only_at_up_is_a_drag() {
        let mut t = Tracker::default();
        t.left_down(0.0, 0.0);
        assert_eq!(t.left_up(0.0, 10.0, 1, false), Some(Gesture::Drag));
    }

    #[test]
    fn drag_state_resets_between_presses() {
        let mut t = Tracker::default();
        t.left_down(0.0, 0.0);
        t.left_dragged(50.0, 0.0);
        assert_eq!(t.left_up(50.0, 0.0, 1, false), Some(Gesture::Drag));
        t.left_down(50.0, 0.0);
        assert_eq!(t.left_up(50.0, 0.0, 1, false), None);
    }

    #[test]
    fn up_without_down_is_never_a_drag() {
        let mut t = Tracker::default();
        t.left_dragged(500.0, 500.0);
        assert_eq!(t.left_up(900.0, 900.0, 1, false), None);
    }

    #[test]
    fn double_and_triple_click() {
        let mut t = Tracker::default();
        t.left_down(10.0, 10.0);
        assert_eq!(t.left_up(10.0, 10.0, 1, false), None);
        t.left_down(10.0, 10.0);
        assert_eq!(t.left_up(10.0, 10.0, 2, false), Some(Gesture::MultiClick));
        t.left_down(10.0, 10.0);
        assert_eq!(t.left_up(10.0, 10.0, 3, false), Some(Gesture::MultiClick));
    }

    #[test]
    fn double_click_then_drag_is_a_drag() {
        let mut t = Tracker::default();
        t.left_down(10.0, 10.0);
        t.left_dragged(80.0, 10.0);
        assert_eq!(t.left_up(80.0, 10.0, 2, true), Some(Gesture::Drag));
    }

    #[test]
    fn shift_click() {
        let mut t = Tracker::default();
        t.left_down(10.0, 10.0);
        assert_eq!(t.left_up(10.0, 10.0, 1, true), Some(Gesture::ShiftClick));
    }

    #[test]
    fn shift_arrow_emits_once_on_key_up() {
        let mut t = Tracker::default();
        // Autorepeat: many downs, one up.
        for _ in 0..20 {
            t.key_down(VK_RIGHT, true, false);
        }
        assert_eq!(t.key_up(VK_RIGHT, true, false), Some(Gesture::Keyboard));
        assert_eq!(t.pending_keys, 0);
    }

    #[test]
    fn shift_released_before_arrow_still_counts() {
        let mut t = Tracker::default();
        t.key_down(VK_LEFT, true, false);
        assert_eq!(t.key_up(VK_LEFT, false, false), Some(Gesture::Keyboard));
        // ...but only once.
        assert_eq!(t.key_up(VK_LEFT, false, false), None);
    }

    #[test]
    fn all_navigation_keys_with_shift() {
        for code in [
            VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN, VK_HOME, VK_END, VK_PAGE_UP, VK_PAGE_DOWN,
        ] {
            let mut t = Tracker::default();
            t.key_down(code, true, false);
            assert_eq!(t.key_up(code, true, false), Some(Gesture::Keyboard), "{code:#x}");
        }
    }

    #[test]
    fn arrows_without_shift_and_ordinary_typing_are_ignored() {
        let mut t = Tracker::default();
        t.key_down(VK_RIGHT, false, false);
        assert_eq!(t.key_up(VK_RIGHT, false, false), None);
        // Shift+letter (typing a capital) is not a selection.
        t.key_down(VK_ANSI_A, true, false);
        assert_eq!(t.key_up(VK_ANSI_A, true, false), None);
        t.key_down(VK_ANSI_C, false, true);
        assert_eq!(t.key_up(VK_ANSI_C, false, true), None);
    }

    #[test]
    fn cmd_a() {
        let mut t = Tracker::default();
        t.key_down(VK_ANSI_A, false, true);
        assert_eq!(t.key_up(VK_ANSI_A, false, true), Some(Gesture::Keyboard));

        // Cmd released first.
        t.key_down(VK_ANSI_A, false, true);
        assert_eq!(t.key_up(VK_ANSI_A, false, false), Some(Gesture::Keyboard));

        // Plain "a" is nothing.
        t.key_down(VK_ANSI_A, false, false);
        assert_eq!(t.key_up(VK_ANSI_A, false, false), None);
    }

    #[test]
    fn out_of_range_keycode_is_harmless() {
        let mut t = Tracker::default();
        t.key_down(300, true, true);
        assert_eq!(t.key_up(300, true, true), None);
    }

    #[test]
    fn middle_click_reports_position() {
        let mut t = Tracker::default();
        assert_eq!(
            t.other_down(2, 12.5, 34.0),
            Some(Event::MiddleClick { x: 12.5, y: 34.0 })
        );
    }

    #[test]
    fn swallowed_middle_down_swallows_drag_and_exactly_one_up() {
        let mut t = Tracker::default();
        assert!(t.other_down(2, 1.0, 2.0).is_some());
        t.middle_verdict(Verdict::Swallow);
        assert_eq!(t.other_dragged(2), Verdict::Swallow);
        assert_eq!(t.other_up(2), Verdict::Swallow);
        assert_eq!(t.other_up(2), Verdict::Pass);
        assert_eq!(t.other_dragged(2), Verdict::Pass);
    }

    #[test]
    fn passed_middle_down_passes_its_up() {
        let mut t = Tracker::default();
        assert!(t.other_down(2, 1.0, 2.0).is_some());
        t.middle_verdict(Verdict::Pass);
        assert_eq!(t.other_dragged(2), Verdict::Pass);
        assert_eq!(t.other_up(2), Verdict::Pass);
    }

    #[test]
    fn swallow_does_not_leak_into_the_next_click() {
        let mut t = Tracker::default();
        // Swallowed down whose up we never saw (e.g. tap was disabled meanwhile).
        t.other_down(2, 0.0, 0.0);
        t.middle_verdict(Verdict::Swallow);
        // Next click is passed: its up must pass too.
        t.other_down(2, 0.0, 0.0);
        t.middle_verdict(Verdict::Pass);
        assert_eq!(t.other_up(2), Verdict::Pass);
    }

    #[test]
    fn other_buttons_always_pass() {
        let mut t = Tracker::default();
        assert_eq!(t.other_down(3, 0.0, 0.0), None);
        assert_eq!(t.other_up(3), Verdict::Pass);

        // Button 3/4 activity in the middle of a swallowed middle click is untouched
        // and doesn't consume the pending swallow.
        t.other_down(2, 0.0, 0.0);
        t.middle_verdict(Verdict::Swallow);
        assert_eq!(t.other_down(4, 0.0, 0.0), None);
        assert_eq!(t.other_dragged(4), Verdict::Pass);
        assert_eq!(t.other_up(4), Verdict::Pass);
        assert_eq!(t.other_up(2), Verdict::Swallow);
    }

    #[test]
    fn keycode_lookup_follows_layout() {
        // QWERTY: ANSI positions match.
        let qwerty = |code: u16| match code {
            VK_ANSI_C => Some('c'),
            VK_ANSI_V => Some('v'),
            _ => None,
        };
        assert_eq!(pick_keycode(qwerty, 'c', VK_ANSI_C), VK_ANSI_C);
        assert_eq!(pick_keycode(qwerty, 'v', VK_ANSI_V), VK_ANSI_V);

        // Dvorak: "c" is on the ANSI I key (0x22), "v" on the ANSI period key (0x2F).
        let dvorak = |code: u16| match code {
            VK_ANSI_C => Some('j'),
            VK_ANSI_V => Some('k'),
            0x22 => Some('c'),
            0x2F => Some('v'),
            _ => None,
        };
        assert_eq!(pick_keycode(dvorak, 'c', VK_ANSI_C), 0x22);
        assert_eq!(pick_keycode(dvorak, 'v', VK_ANSI_V), 0x2F);

        // Nothing produces the character: keep the ANSI position.
        assert_eq!(pick_keycode(|_| None, 'c', VK_ANSI_C), VK_ANSI_C);
    }

    #[test]
    fn event_mask_constants() {
        // Guard against typos in the values checked against CGEventTypes.h / IOLLEvent.h.
        assert_eq!(
            (EV_LEFT_MOUSE_DOWN, EV_LEFT_MOUSE_UP, EV_LEFT_MOUSE_DRAGGED),
            (1, 2, 6)
        );
        assert_eq!((EV_KEY_DOWN, EV_KEY_UP), (10, 11));
        assert_eq!(
            (EV_OTHER_MOUSE_DOWN, EV_OTHER_MOUSE_UP, EV_OTHER_MOUSE_DRAGGED),
            (25, 26, 27)
        );
    }
}
