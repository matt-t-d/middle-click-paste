mod ax;
mod ffi;
mod input;
mod launchd;
mod pasteboard;

fn main() {
    // Wired up in a later commit.
    let _ = (
        ax::is_trusted as fn(bool) -> bool,
        input::post_copy as fn(),
        pasteboard::change_count as fn() -> i64,
        launchd::uninstall as fn() -> Result<(), String>,
    );
}
