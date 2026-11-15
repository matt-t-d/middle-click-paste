//! LaunchAgent install/uninstall so the daemon starts at login.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

const LABEL: &str = "com.github.middle-click-paste";
const LAUNCHCTL: &str = "/bin/launchctl";

extern "C" {
    fn getuid() -> u32;
}

/// Where `install` puts the binary the LaunchAgent runs, relative to $HOME.
///
/// Accessibility permission is tied to the binary's (ad-hoc) code signature, which
/// changes on every build. Running a copy that only `install` replaces keeps the
/// grant across `cargo build`s.
const INSTALL_PATH: &str = ".local/bin/middle-click-paste";

/// Copy the current executable to [`INSTALL_PATH`], write
/// ~/Library/LaunchAgents/<label>.plist pointing at the copy (with `extra_args`
/// appended after the `run` subcommand) and load it.
/// Returns the installed binary's path and what happened to it.
pub fn install(extra_args: &[String]) -> Result<(PathBuf, Installed), String> {
    let home = home_dir()?;
    let current = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot determine path of the current executable: {e}"))?;
    let installed = home.join(INSTALL_PATH);
    let outcome = copy_binary(&current, &installed, sign)?;
    let exe = installed
        .to_str()
        .ok_or("install path is not valid UTF-8")?
        .to_string();

    let agents_dir = home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents_dir)
        .map_err(|e| format!("cannot create {}: {e}", agents_dir.display()))?;
    let plist_path = agents_dir.join(format!("{LABEL}.plist"));
    let log_path = home.join("Library/Logs/middle-click-paste.log");
    if let Some(logs_dir) = log_path.parent() {
        // Best effort; launchd also creates missing log parents on its own.
        let _ = fs::create_dir_all(logs_dir);
    }
    let log_path = log_path
        .to_str()
        .ok_or("home directory path is not valid UTF-8")?
        .to_string();

    let plist = generate_plist(&exe, extra_args, &log_path);
    fs::write(&plist_path, plist)
        .map_err(|e| format!("cannot write {}: {e}", plist_path.display()))?;

    let domain = domain_target();
    // Idempotent: unload any existing instance first.
    if is_loaded()? {
        bootout()?;
    }

    let plist_str = plist_path
        .to_str()
        .ok_or("plist path is not valid UTF-8")?
        .to_string();
    // bootout can complete asynchronously; retry bootstrap briefly.
    let mut last_err = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            sleep(Duration::from_millis(400));
        }
        let out = launchctl(&["bootstrap", &domain, &plist_str])?;
        if out.status.success() {
            return Ok((installed, outcome));
        }
        last_err = describe_failure(&out);
    }
    Err(format!(
        "launchctl bootstrap failed for {}: {last_err}",
        plist_path.display()
    ))
}

/// Unload and remove the LaunchAgent.
pub fn uninstall() -> Result<(), String> {
    if is_loaded()? {
        bootout()?;
    }
    let plist_path = home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    remove_if_present(&plist_path)?;
    remove_if_present(&home_dir()?.join(INSTALL_PATH))
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

/// Self-signed code-signing identity created by `scripts/create-signing-cert.sh`.
pub const SIGNING_IDENTITY: &str = "middle-click-paste dev";

/// Sign `path` with [`SIGNING_IDENTITY`] and a fixed identifier, so macOS keys the
/// Accessibility grant to the certificate rather than the build's content hash.
/// False if the identity doesn't exist or signing fails.
fn sign(path: &Path) -> bool {
    Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", SIGNING_IDENTITY, "--identifier", LABEL])
        .arg(path)
        .output()
        .map_or(false, |out| out.status.success())
}

/// What `install` did with the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    /// The installed file changed.
    pub changed: bool,
    /// Signed with [`SIGNING_IDENTITY`]: the Accessibility grant carries over.
    pub signed: bool,
}

/// Copy `src` to `dest` and `sign` the copy. The new file is written next to
/// `dest` and renamed over it, so a running copy is never modified in place. If
/// the result is identical to `dest`, `dest` is left alone.
fn copy_binary(src: &Path, dest: &Path, sign: impl Fn(&Path) -> bool) -> Result<Installed, String> {
    if src == dest {
        return Ok(Installed { changed: false, signed: sign(dest) });
    }
    let bytes = fs::read(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    let dir = dest.parent().ok_or("install path has no parent directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".middle-click-paste.{}.tmp", std::process::id()));
    let fail = |e: std::io::Error| {
        let _ = fs::remove_file(&tmp);
        format!("cannot install binary to {}: {e}", dest.display())
    };
    fs::write(&tmp, &bytes)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)))
        .map_err(fail)?;
    let signed = sign(&tmp);
    let new = fs::read(&tmp).map_err(fail)?;
    if fs::read(dest).map_or(false, |existing| existing == new) {
        let _ = fs::remove_file(&tmp);
        return Ok(Installed { changed: false, signed });
    }
    fs::rename(&tmp, dest).map_err(fail)?;
    Ok(Installed { changed: true, signed })
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn domain_target() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { getuid() })
}

fn service_target() -> String {
    format!("{}/{LABEL}", domain_target())
}

fn launchctl(args: &[&str]) -> Result<Output, String> {
    Command::new(LAUNCHCTL)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {LAUNCHCTL}: {e}"))
}

fn describe_failure(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("exit status {}", out.status)
    } else {
        stderr.to_string()
    }
}

/// True if an agent with our label is currently loaded in the user's GUI domain.
fn is_loaded() -> Result<bool, String> {
    Ok(launchctl(&["print", &service_target()])?.status.success())
}

fn bootout() -> Result<(), String> {
    let out = launchctl(&["bootout", &service_target()])?;
    if out.status.success() {
        return Ok(());
    }
    // Raced with the agent going away: treat "not loaded" as success.
    if !is_loaded()? {
        return Ok(());
    }
    Err(format!(
        "launchctl bootout {} failed: {}",
        service_target(),
        describe_failure(&out)
    ))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Pure plist generation: executable path + extra args + log path in, XML out.
fn generate_plist(exe: &str, extra_args: &[String], log_path: &str) -> String {
    let mut args = String::new();
    for a in std::iter::once(exe)
        .chain(std::iter::once("run"))
        .chain(extra_args.iter().map(|s| s.as_str()))
    {
        args.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(a)));
    }
    let log = xml_escape(log_path);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{label}</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
{args}\t</array>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>KeepAlive</key>\n\
\t<dict>\n\
\t\t<key>SuccessfulExit</key>\n\
\t\t<false/>\n\
\t</dict>\n\
\t<key>ProcessType</key>\n\
\t<string>Interactive</string>\n\
\t<key>StandardOutPath</key>\n\
\t<string>{log}</string>\n\
\t<key>StandardErrorPath</key>\n\
\t<string>{log}</string>\n\
</dict>\n\
</plist>\n",
        label = xml_escape(LABEL),
        args = args,
        log = log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        generate_plist(
            "/Applications/A&B <x>/mcp",
            &["--flag".to_string(), "a>b".to_string()],
            "/Users/me/Library/Logs/middle-click-paste.log",
        )
    }

    #[test]
    fn structure() {
        let p = sample();
        assert!(p.starts_with("<?xml"));
        assert!(p.contains("<key>Label</key>\n\t<string>com.github.middle-click-paste</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(p.contains("<key>ProcessType</key>\n\t<string>Interactive</string>"));
        assert!(p.contains("<key>StandardOutPath</key>"));
        assert!(p.contains("<key>StandardErrorPath</key>"));
        assert!(p.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn args_in_order() {
        let p = sample();
        let exe = p.find("<string>/Applications/A&amp;B &lt;x&gt;/mcp</string>").unwrap();
        let run = p.find("<string>run</string>").unwrap();
        let f1 = p.find("<string>--flag</string>").unwrap();
        let f2 = p.find("<string>a&gt;b</string>").unwrap();
        assert!(exe < run && run < f1 && f1 < f2);
    }

    #[test]
    fn escaping() {
        let p = sample();
        assert!(!p.contains("A&B"));
        assert!(!p.contains("<x>"));
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
    }

    #[test]
    fn no_extra_args() {
        let p = generate_plist("/bin/x", &[], "/tmp/l");
        assert!(p.contains("<string>/bin/x</string>\n\t\t<string>run</string>\n\t</array>"));
    }

    #[test]
    fn copy_binary_skips_identical_and_replaces_changed() {
        let dir = std::env::temp_dir().join(format!("mcp-copy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let src = dir.join("src-bin");
        let dest = dir.join("sub/dest-bin");
        fs::create_dir_all(&dir).unwrap();

        let unsigned = |_: &Path| false;
        let copy = |sign: &dyn Fn(&Path) -> bool| copy_binary(&src, &dest, sign).unwrap();
        let changed = |signed| Installed { changed: true, signed };
        let same = |signed| Installed { changed: false, signed };

        fs::write(&src, b"v1").unwrap();
        assert_eq!(copy(&unsigned), changed(false), "first copy");
        assert_eq!(fs::read(&dest).unwrap(), b"v1");
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(copy(&unsigned), same(false), "identical: untouched");
        fs::write(&src, b"v2").unwrap();
        assert_eq!(copy(&unsigned), changed(false), "changed: replaced");
        assert_eq!(fs::read(&dest).unwrap(), b"v2");

        // The signer runs on the copy, before it replaces dest.
        let fake_sign = |p: &Path| fs::write(p, b"v2+sig").is_ok();
        assert_eq!(copy(&fake_sign), changed(true));
        assert_eq!(fs::read(&dest).unwrap(), b"v2+sig");
        assert_eq!(fs::read(&src).unwrap(), b"v2", "source untouched");
        assert_eq!(copy(&fake_sign), same(true), "identical after signing: untouched");

        assert_eq!(copy_binary(&dest, &dest, unsigned).unwrap(), same(false), "self-copy");
        assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 1, "no temp left");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Writes a sample for manual `plutil -lint` when MCP_PLIST_OUT is set.
    #[test]
    fn dump_sample() {
        if let Ok(path) = std::env::var("MCP_PLIST_OUT") {
            std::fs::write(path, sample()).unwrap();
        }
    }
}
