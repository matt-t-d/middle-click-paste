//! Accessibility (AXUIElement) queries.
//! STUB: signatures are a fixed contract; implementation pending.

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

/// Whether this process has Accessibility permission. `prompt` shows the system dialog if not.
pub fn is_trusted(_prompt: bool) -> bool {
    todo!()
}

/// Selected text of the system-wide focused element.
pub fn selected_text() -> Selection {
    todo!()
}

/// Classify the element at global screen coordinates (top-left origin, as in CGEvent locations).
/// Must return quickly (<= ~150ms worst case): it is called from the event-tap callback.
pub fn target_at(_x: f64, _y: f64) -> Target {
    todo!()
}
