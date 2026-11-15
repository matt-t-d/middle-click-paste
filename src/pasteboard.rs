//! System (general) pasteboard access via the Objective-C runtime.
//! STUB: signatures are a fixed contract; implementation pending.

/// Full copy of the general pasteboard's contents (every item, every type).
pub struct Snapshot {}

/// `NSPasteboard.general.changeCount`.
pub fn change_count() -> i64 {
    todo!()
}

/// Plain-text contents of the general pasteboard, if any.
pub fn read_string() -> Option<String> {
    todo!()
}

/// Capture everything currently on the general pasteboard.
pub fn snapshot() -> Snapshot {
    todo!()
}

/// Put a snapshot back, replacing whatever is there now.
pub fn restore(_snap: &Snapshot) {
    todo!()
}

/// Replace the pasteboard with `text`, flagged as transient/auto-generated
/// (nspasteboard.org conventions) so clipboard managers ignore it.
pub fn write_transient_string(_text: &str) {
    todo!()
}
