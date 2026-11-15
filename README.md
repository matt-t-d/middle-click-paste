# middle-click-paste

Linux-style select-to-copy and middle-click-to-paste for macOS.

Select text anywhere and it is stored in a private buffer. Middle-click a text
field and that buffer is pasted at the pointer. The normal clipboard (Cmd+C /
Cmd+V) keeps its own contents, so you effectively have two clipboards.

Written in Rust with no dependencies; macOS APIs are called through
hand-written FFI.

## Install

```sh
cargo build --release
./target/release/middle-click-paste            # run in the foreground
./target/release/middle-click-paste install    # copy to ~/.local/bin, start now and at every login
./target/release/middle-click-paste uninstall  # remove the LaunchAgent and the copy
```

It needs Accessibility permission (System Settings > Privacy & Security >
Accessibility). On first run macOS prompts for it and the program waits until
it is granted. Run from a terminal, the permission belongs to the terminal app;
installed as a LaunchAgent, it belongs to `~/.local/bin/middle-click-paste`.
The grant is tied to that exact binary, so `cargo build` doesn't affect the
installed daemon. Running `install` after a code change replaces the copy, and
then the grant has to be redone: remove the old entry and add the binary again.
`install` leaves an identical copy untouched, so its grant stays valid.

Logs for the LaunchAgent go to `~/Library/Logs/middle-click-paste.log`.

## Options

| Flag | Effect |
| --- | --- |
| `--always` | Paste on every middle-click, not just over editable text. Breaks middle-click on links and tabs. |
| `--strict` | Paste only where Accessibility positively reports editable text. By default, windows it cannot see into (Alacritty, kitty, ...) also get pastes. |
| `--no-copy-fallback` | Never send Cmd+C to read a selection; use Accessibility only. |
| `-v`, `--verbose` | Log captures and pastes to stderr. |

Options given to `install` are passed to the daemon.

## How it works

- **Capture.** An event tap watches for gestures that usually make a selection:
  drag, double/triple click, shift-click, shift+arrow keys and Cmd+A. After
  one, the selected text is read from the focused element through the
  Accessibility API. If the app does not expose its selection that way, the
  program sends Cmd+C, reads the result, and restores the clipboard to exactly
  what it held before. Deselecting does not clear the buffer.
- **Paste.** On a middle-click over an editable text element, or in a window
  Accessibility cannot see into (and only if the buffer is non-empty), the click is swallowed. The program left-clicks at the
  pointer to place the caret, puts the buffer on the clipboard, sends Cmd+V,
  and then restores the clipboard. Middle-clicks anywhere else pass through
  untouched.

## Limitations

- The clipboard is borrowed for about a quarter of a second during a paste (and
  during a Cmd+C fallback capture). The borrowed content is marked transient
  per nspasteboard.org so clipboard managers ignore it.
- Windows that expose nothing to Accessibility (Alacritty, kitty, some games)
  cannot be inspected, so a middle-click anywhere in them pastes. `--strict`
  disables that.
- Chromium and Electron apps build their accessibility tree lazily; until it is
  on, text fields in them may not be recognised. `--always` is the workaround.
- The Cmd+C fallback can fire after a drag that was not a text selection in
  such apps. `--no-copy-fallback` turns it off.
- Password fields are never captured.
