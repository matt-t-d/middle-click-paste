//! LaunchAgent install/uninstall so the daemon starts at login.
//! STUB: signatures are a fixed contract; implementation pending.

/// Write ~/Library/LaunchAgents/<label>.plist pointing at the current executable
/// (with `extra_args` appended after the `run` subcommand) and load it.
pub fn install(_extra_args: &[String]) -> Result<(), String> {
    todo!()
}

/// Unload and remove the LaunchAgent.
pub fn uninstall() -> Result<(), String> {
    todo!()
}
