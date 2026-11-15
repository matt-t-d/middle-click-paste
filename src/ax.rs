//! Accessibility (AXUIElement) queries.
//!
//! All functions are safe to call from any thread: every call creates and
//! releases its own AX/CF objects and no state is shared.
#![allow(non_upper_case_globals, non_snake_case)]

use crate::ffi::{
    cfstring, cfstring_to_string, Boolean, CFAllocatorRef, CFGetTypeID, CFIndex, CFRelease,
    CFRetain, CFStringRef, CFTypeID, CFTypeRef,
};
use std::ffi::{c_char, c_void};
use std::time::{Duration, Instant};

/// Result of asking the focused UI element for its selected text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Non-empty selected text.
    Text(String),
    /// The focused element exposes selected text and it is empty.
    Empty,
    /// No focused element, or it doesn't expose selected text via AX.
    Unsupported,
}

/// What is under a screen point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// An editable text element (text field/area, editable web content, terminal view...).
    Text,
    /// AX positively identified something that is not an editable text element.
    NotText,
    /// AX gave no usable answer (app doesn't support AX, timeout, ...).
    Unknown,
}

// ---------------------------------------------------------------------------
// FFI
// ---------------------------------------------------------------------------

type AXUIElementRef = *const c_void;
type AXError = i32;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;

const kAXErrorSuccess: AXError = 0;
const kAXErrorCannotComplete: AXError = -25204;
const kAXErrorAPIDisabled: AXError = -25211;
const kAXErrorNoValue: AXError = -25212;

/// Opaque CF callback structs; only their addresses are used.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;

    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, seconds: f32) -> AXError;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        parameter: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementCopyMultipleAttributeValues(
        element: AXUIElementRef,
        attributes: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AXError;
    fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> AXError;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    static kCFTypeArrayCallBacks: Opaque;

    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: CFIndex,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> CFDictionaryRef;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const CFTypeRef,
        count: CFIndex,
        callbacks: *const Opaque,
    ) -> CFArrayRef;
    fn CFArrayGetTypeID() -> CFTypeID;
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> CFTypeRef;
}

// Objective-C runtime, used only to ask NSWorkspace for the frontmost app.
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *const c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

// ---------------------------------------------------------------------------
// Ownership helpers
// ---------------------------------------------------------------------------

/// Owned (+1) CF object, released on drop. Never null.
struct Cf(CFTypeRef);

impl Cf {
    /// Take ownership of a +1 reference; `None` if null.
    fn owned(p: CFTypeRef) -> Option<Cf> {
        if p.is_null() {
            None
        } else {
            Some(Cf(p))
        }
    }

    /// Retain a borrowed reference; `None` if null.
    fn retained(p: CFTypeRef) -> Option<Cf> {
        if p.is_null() {
            None
        } else {
            Some(Cf(unsafe { CFRetain(p) }))
        }
    }

    fn is_element(&self) -> bool {
        unsafe { CFGetTypeID(self.0) == AXUIElementGetTypeID() }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

/// Timeout applied process-wide to AX messaging (via the system-wide element).
const GLOBAL_TIMEOUT_SECS: f32 = 0.12;
/// Total wall-clock budget for `target_at`.
const TARGET_BUDGET: Duration = Duration::from_millis(150);
/// Per-element timeout for reading the selection from the focused element.
const SELECTION_TIMEOUT_SECS: f32 = 0.25;
/// Maximum number of elements inspected by `target_at` (hit element + ancestors).
const MAX_WALK: usize = 10;

/// System-wide element with the short messaging timeout applied. Setting the
/// timeout on the system-wide element sets the default for the whole process.
fn system_wide() -> Option<Cf> {
    let sys = Cf::owned(unsafe { AXUIElementCreateSystemWide() })?;
    unsafe { AXUIElementSetMessagingTimeout(sys.0, GLOBAL_TIMEOUT_SECS) };
    Some(sys)
}

fn copy_attr(element: &Cf, name: &str) -> Result<Cf, AXError> {
    let attr = Cf::owned(cfstring(name)).ok_or(kAXErrorCannotComplete)?;
    let mut value: CFTypeRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(element.0, attr.0, &mut value) };
    let value = Cf::owned(value);
    if err != kAXErrorSuccess {
        return Err(err);
    }
    value.ok_or(kAXErrorNoValue)
}

fn copy_string_attr(element: &Cf, name: &str) -> Option<String> {
    copy_attr(element, name)
        .ok()
        .and_then(|v| cfstring_to_string(v.0))
}

fn copy_element_attr(element: &Cf, name: &str) -> Result<Cf, AXError> {
    let v = copy_attr(element, name)?;
    if v.is_element() {
        Ok(v)
    } else {
        Err(kAXErrorNoValue)
    }
}

// ---------------------------------------------------------------------------
// is_trusted
// ---------------------------------------------------------------------------

/// Whether this process has Accessibility permission. `prompt` shows the system dialog if not.
pub fn is_trusted(prompt: bool) -> bool {
    unsafe {
        let key: CFTypeRef = kAXTrustedCheckOptionPrompt;
        let value: CFTypeRef = if prompt {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        };
        let options = Cf::owned(CFDictionaryCreate(
            std::ptr::null(),
            &key,
            &value,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ));
        match options {
            Some(options) => AXIsProcessTrustedWithOptions(options.0) != 0,
            None => AXIsProcessTrustedWithOptions(std::ptr::null()) != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// selected_text
// ---------------------------------------------------------------------------

/// PID of the frontmost application according to NSWorkspace.
fn frontmost_pid() -> Option<i32> {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let pid = (|| {
            let class = objc_getClass(b"NSWorkspace\0".as_ptr() as *const c_char);
            if class.is_null() {
                return None;
            }
            let send_id: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_pid: unsafe extern "C" fn(*mut c_void, *const c_void) -> i32 =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let workspace = send_id(class, sel_registerName(b"sharedWorkspace\0".as_ptr() as *const c_char));
            if workspace.is_null() {
                return None;
            }
            let app = send_id(
                workspace,
                sel_registerName(b"frontmostApplication\0".as_ptr() as *const c_char),
            );
            if app.is_null() {
                return None;
            }
            let pid = send_pid(app, sel_registerName(b"processIdentifier\0".as_ptr() as *const c_char));
            if pid > 0 {
                Some(pid)
            } else {
                None
            }
        })();
        objc_autoreleasePoolPop(pool);
        pid
    }
}

/// The focused UI element: system-wide lookup first, then via the focused /
/// frontmost application's own AX element (some apps only answer that way).
fn focused_element() -> Option<Cf> {
    let sys = system_wide()?;
    match copy_element_attr(&sys, "AXFocusedUIElement") {
        Ok(el) => return Some(el),
        // No permission: nothing else will work either.
        Err(kAXErrorAPIDisabled) => return None,
        Err(_) => {}
    }
    if let Ok(app) = copy_element_attr(&sys, "AXFocusedApplication") {
        if let Ok(el) = copy_element_attr(&app, "AXFocusedUIElement") {
            return Some(el);
        }
    }
    let app = Cf::owned(unsafe { AXUIElementCreateApplication(frontmost_pid()?) })?;
    copy_element_attr(&app, "AXFocusedUIElement").ok()
}

/// Selection via text markers (WebKit / Chromium web content, where the focused
/// element is often an AXWebArea that doesn't answer AXSelectedText).
fn selected_text_via_markers(element: &Cf) -> Option<String> {
    let range = copy_attr(element, "AXSelectedTextMarkerRange").ok()?;
    let attr = Cf::owned(cfstring("AXStringForTextMarkerRange"))?;
    let mut value: CFTypeRef = std::ptr::null();
    let err = unsafe {
        AXUIElementCopyParameterizedAttributeValue(element.0, attr.0, range.0, &mut value)
    };
    let value = Cf::owned(value)?;
    if err != kAXErrorSuccess {
        return None;
    }
    cfstring_to_string(value.0)
}

fn is_text_entry_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox")
}

/// Selected text of the system-wide focused element.
pub fn selected_text() -> Selection {
    let Some(element) = focused_element() else {
        return Selection::Unsupported;
    };
    unsafe { AXUIElementSetMessagingTimeout(element.0, SELECTION_TIMEOUT_SECS) };

    // Never read from password fields, and report Empty so the caller doesn't
    // fall back to a synthetic copy either.
    if copy_string_attr(&element, "AXSubrole").as_deref() == Some("AXSecureTextField") {
        return Selection::Empty;
    }

    match copy_attr(&element, "AXSelectedText") {
        Ok(value) => match cfstring_to_string(value.0) {
            Some(s) if s.is_empty() => Selection::Empty,
            Some(s) => Selection::Text(s),
            None => Selection::Unsupported,
        },
        // Timeout / app not answering: don't issue further requests.
        Err(kAXErrorCannotComplete) => Selection::Unsupported,
        Err(err) => match selected_text_via_markers(&element) {
            Some(s) if s.is_empty() => Selection::Empty,
            Some(s) => Selection::Text(s),
            None => {
                // "No value" from a genuine text-entry element means AX works
                // there and nothing is selected. From anything else it is too
                // weak a signal to suppress the caller's fallback.
                let text_entry = err == kAXErrorNoValue
                    && copy_string_attr(&element, "AXRole")
                        .map_or(false, |role| is_text_entry_role(&role));
                if text_entry {
                    Selection::Empty
                } else {
                    Selection::Unsupported
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// target_at
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Editable text: a paste belongs here.
    Text,
    /// Positively not text, and nothing above it can change that
    /// (links, buttons, tabs, menus, images, containers bounding the search).
    Stop,
    /// Neutral content (static text, groups, lists...): look at the parent.
    Pass,
}

fn classify(role: &str, subrole: Option<&str>) -> Kind {
    if is_text_entry_role(role) {
        return Kind::Text;
    }
    if matches!(subrole, Some("AXSearchField" | "AXSecureTextField")) {
        return Kind::Text;
    }
    match role {
        // Things where a middle-click has (or may have) its own meaning, or
        // that are plainly controls.
        "AXLink" | "AXButton" | "AXPopUpButton" | "AXMenuButton" | "AXRadioButton"
        | "AXCheckBox" | "AXImage" | "AXMenu" | "AXMenuItem" | "AXMenuBar" | "AXMenuBarItem"
        | "AXTabGroup" | "AXToolbar" | "AXSlider" | "AXScrollBar" | "AXIncrementor"
        | "AXDisclosureTriangle" | "AXColorWell" | "AXDockItem"
        // Boundaries: an editable ancestor beyond these doesn't make the hit
        // element editable (e.g. the popup list of a combo box).
        | "AXScrollArea" | "AXSplitGroup" | "AXWebArea" | "AXWindow" | "AXSheet" | "AXDrawer"
        | "AXApplication" => Kind::Stop,
        _ => Kind::Pass,
    }
}

struct Info {
    role: Option<String>,
    subrole: Option<String>,
    parent: Option<Cf>,
}

/// Role, subrole and parent of `element` in a single round trip.
/// `attrs` is a CFArray of [AXRole, AXSubrole, AXParent].
fn describe(element: &Cf, attrs: &Cf) -> Result<Info, AXError> {
    let mut values: CFArrayRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyMultipleAttributeValues(element.0, attrs.0, 0, &mut values) };
    let values = Cf::owned(values);
    if err == kAXErrorSuccess {
        if let Some(values) = values {
            unsafe {
                if CFGetTypeID(values.0) == CFArrayGetTypeID() && CFArrayGetCount(values.0) == 3 {
                    // Unavailable attributes come back as AXValue error
                    // placeholders, which fail the type checks below.
                    let parent = Cf::retained(CFArrayGetValueAtIndex(values.0, 2))
                        .filter(|p| p.is_element());
                    return Ok(Info {
                        role: cfstring_to_string(CFArrayGetValueAtIndex(values.0, 0)),
                        subrole: cfstring_to_string(CFArrayGetValueAtIndex(values.0, 1)),
                        parent,
                    });
                }
            }
        }
    } else if err == kAXErrorCannotComplete || err == kAXErrorAPIDisabled {
        return Err(err);
    }
    // The batched call isn't answered by this element: ask one at a time.
    let role = copy_attr(element, "AXRole")?;
    Ok(Info {
        role: cfstring_to_string(role.0),
        subrole: copy_string_attr(element, "AXSubrole"),
        parent: copy_element_attr(element, "AXParent").ok(),
    })
}

fn walk_attrs() -> Option<Cf> {
    let names = [
        Cf::owned(cfstring("AXRole"))?,
        Cf::owned(cfstring("AXSubrole"))?,
        Cf::owned(cfstring("AXParent"))?,
    ];
    let raw = [names[0].0, names[1].0, names[2].0];
    Cf::owned(unsafe { CFArrayCreate(std::ptr::null(), raw.as_ptr(), 3, &kCFTypeArrayCallBacks) })
}

/// Whether `window` exposes nothing to AX beyond its title-bar buttons, i.e.
/// its content is invisible to accessibility. Also true if the budget runs out.
fn window_is_opaque(window: &Cf, deadline: Instant) -> bool {
    let Ok(children) = copy_attr(window, "AXChildren") else {
        return true;
    };
    unsafe {
        if CFGetTypeID(children.0) != CFArrayGetTypeID() {
            return true;
        }
        let count = CFArrayGetCount(children.0);
        if count > 6 {
            return false;
        }
        for i in 0..count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining < Duration::from_millis(5) {
                return true;
            }
            let Some(child) = Cf::retained(CFArrayGetValueAtIndex(children.0, i)) else {
                continue;
            };
            if !child.is_element() {
                continue;
            }
            AXUIElementSetMessagingTimeout(child.0, remaining.as_secs_f32());
            match copy_attr(&child, "AXRole") {
                Ok(role) => {
                    if cfstring_to_string(role.0).as_deref() != Some("AXButton") {
                        return false;
                    }
                }
                Err(kAXErrorCannotComplete) => return true,
                Err(_) => {}
            }
        }
    }
    true
}

/// Classify the element at global screen coordinates (top-left origin, as in CGEvent locations).
/// Must return quickly (<= ~150ms worst case): it is called from the event-tap callback.
pub fn target_at(x: f64, y: f64) -> Target {
    let deadline = Instant::now() + TARGET_BUDGET;
    let Some(sys) = system_wide() else {
        return Target::Unknown;
    };
    let mut hit: AXUIElementRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyElementAtPosition(sys.0, x as f32, y as f32, &mut hit) };
    let hit = Cf::owned(hit);
    if err != kAXErrorSuccess {
        return Target::Unknown;
    }
    let (Some(mut element), Some(attrs)) = (hit, walk_attrs()) else {
        return Target::Unknown;
    };

    for depth in 0..MAX_WALK {
        // Give each request only what is left of the overall budget.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_millis(5) {
            return Target::Unknown;
        }
        unsafe { AXUIElementSetMessagingTimeout(element.0, remaining.as_secs_f32()) };

        let info = match describe(&element, &attrs) {
            Ok(info) => info,
            Err(_) => return Target::Unknown,
        };
        let Some(role) = info.role else {
            // An element with no role tells us nothing by itself.
            return if depth == 0 {
                Target::Unknown
            } else {
                Target::NotText
            };
        };
        if depth == 0 && role == "AXWindow" {
            // The hit landed on the window itself. Either it's window chrome /
            // background, or the app draws its content without exposing it to
            // AX (Alacritty, kitty, games...) and AX can't tell us anything.
            return if window_is_opaque(&element, deadline) {
                Target::Unknown
            } else {
                Target::NotText
            };
        }
        match classify(&role, info.subrole.as_deref()) {
            Kind::Text => return Target::Text,
            Kind::Stop => return Target::NotText,
            Kind::Pass => match info.parent {
                Some(parent) => element = parent,
                None => return Target::NotText,
            },
        }
    }
    Target::NotText
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_rules() {
        assert_eq!(classify("AXTextField", None), Kind::Text);
        assert_eq!(classify("AXTextField", Some("AXSearchField")), Kind::Text);
        assert_eq!(classify("AXTextField", Some("AXSecureTextField")), Kind::Text);
        assert_eq!(classify("AXTextArea", None), Kind::Text);
        assert_eq!(classify("AXComboBox", None), Kind::Text);
        assert_eq!(classify("AXGroup", Some("AXSearchField")), Kind::Text);

        assert_eq!(classify("AXLink", None), Kind::Stop);
        assert_eq!(classify("AXButton", None), Kind::Stop);
        assert_eq!(classify("AXRadioButton", Some("AXTabButton")), Kind::Stop);
        assert_eq!(classify("AXImage", None), Kind::Stop);
        assert_eq!(classify("AXMenuBarItem", None), Kind::Stop);
        assert_eq!(classify("AXWebArea", None), Kind::Stop);
        assert_eq!(classify("AXWindow", Some("AXStandardWindow")), Kind::Stop);

        assert_eq!(classify("AXStaticText", None), Kind::Pass);
        assert_eq!(classify("AXGroup", None), Kind::Pass);
        assert_eq!(classify("AXList", None), Kind::Pass);
        assert_eq!(classify("AXCell", None), Kind::Pass);
    }

    #[test]
    fn is_trusted_does_not_crash() {
        let a = is_trusted(false);
        let b = is_trusted(false);
        assert_eq!(a, b);
    }

    #[test]
    fn selected_text_returns_in_time() {
        let start = Instant::now();
        let selection = selected_text();
        let elapsed = start.elapsed();
        let kind = match &selection {
            Selection::Text(s) => format!("Text({} bytes)", s.len()),
            other => format!("{other:?}"),
        };
        eprintln!("selected_text: {kind} trusted={} elapsed={elapsed:?}", is_trusted(false));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
        match selection {
            Selection::Text(s) => assert!(!s.is_empty()),
            Selection::Empty => assert!(is_trusted(false)),
            Selection::Unsupported => {}
        }
    }

    #[test]
    fn target_at_returns_in_time() {
        let trusted = is_trusted(false);
        // (10, 10) is in the menu bar (Apple menu) on the main display.
        for &(x, y) in &[(10.0, 10.0), (-50_000.0, -50_000.0), (400.0, 400.0)] {
            let start = Instant::now();
            let target = target_at(x, y);
            let elapsed = start.elapsed();
            eprintln!("target_at({x}, {y}) = {target:?} trusted={trusted} elapsed={elapsed:?}");
            assert!(elapsed < Duration::from_millis(400), "took {elapsed:?}");
            if !trusted {
                assert_eq!(target, Target::Unknown);
            }
        }
        if trusted {
            // Unknown is tolerated (hidden menu bar, locked screen), Text never.
            assert_ne!(target_at(10.0, 10.0), Target::Text);
            assert_ne!(target_at(-50_000.0, -50_000.0), Target::Text);
        }
    }

    #[test]
    fn concurrent_calls_do_not_crash() {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        let _ = target_at(10.0 + i as f64, 10.0);
                        let _ = selected_text();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn frontmost_pid_does_not_crash() {
        let _ = frontmost_pid();
    }
}
