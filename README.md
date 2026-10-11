# Terminal implementation notes

## What the code does

- `src/pty.rs` owns the terminal lifecycle: one `portable-pty` child per tab,
  a reader thread forwarding bytes into an `Outbox`, a writer thread handling
  input and resize commands, and a single async pump updating xterm in the
  webview. The JavaScript bridge carries input and resize events back to Rust.
- `assets/xterm.js` and `assets/xterm.css` are bundled xterm.js distribution
  files, not application logic. `assets/terminal/` contains focused JavaScript
  templates for the bridge, xterm lifecycle, and theme. `assets/app.css` styles
  the application shell and `assets/terminal.css` owns terminal-pane styling.
- `src/main.rs` owns the window, menu, app layout, and the weak PTY cleanup
  registry. `src/util.rs` contains shared base64 and JavaScript-string helpers.
- The active shell's working directory is the workspace source of truth.
  Supported shell prompts report it with OSC 7; xterm parses that control
  sequence and sends a `cwd` bridge event to Rust. Rust updates the active
  terminal tab and the file-tree root. Clicking a file opens it in the editor
  without moving the shell.
- Directory enumeration, editor file reads, and file saves run on Tokio's
  blocking pool. The UI thread applies completed results and remains the only
  thread that calls the WebView.

## Why it is more than a single terminal widget

The Rust application sources are about 1,900 lines; the xterm.js bundle alone
is about 489 KB and is not hand-written terminal logic. The app code also
includes multi-tab state, PTY process/thread coordination, input/output
forwarding, resizing, theme changes, window menus, and UI styling. A simpler
single-tab implementation can be much shorter by omitting those features.

`pty.rs` still coordinates the session lifecycle, UI events, and output pump,
but the JavaScript and CSS resources live in focused assets so visual and
bridge changes do not need to disturb the PTY plumbing.

## Debugging order

When the terminal appears blank or does not accept input, trace the path in
this order:

1. Check whether `PtySession::new` succeeds and which shell
   `shell_command()` selected.
2. Confirm `xterm_boot_js()` loads the bundled `Terminal` constructor and the
   active `xterm-root-N` element exists.
3. Confirm the eval bridge is receiving `ready`, then `size` and `in` messages.
4. Confirm the PTY writer receives input and the reader adds output to `Outbox`.
5. Confirm `pump_output` sends the output to the matching xterm instance.
6. For a stale file tree, verify the shell prompt emits OSC 7 and the `cwd`
   bridge event updates the active tab.

The built-in cwd prompt reporting is configured for PowerShell, `cmd.exe`, and
Bash. Other shells need their own OSC 7 prompt integration; without it, the
workspace remains at that tab's last reported directory.

The reader must publish each successful PTY read immediately. Buffering until
`READ_BUF` fills stalls the visible interaction in practice: an interactive
shell's prompt and most command output are much smaller than that threshold.
Batching belongs in `pump_output`, which already coalesces short bursts.

Build checks catch Rust errors but do not exercise a native WebView, shell
startup, or keyboard I/O. For those, run the desktop app and test a command that
prints a known marker, then test resize, tab switching, and closing the window.
`cargo test pty_round_trips_shell_output` checks the selected shell's PTY
input/output path, but does not test xterm rendering or the WebView bridge.

## Lifecycle invariants

- Closing a tab or unmounting the terminal must terminate its PTY child.
- Unmounting cancels the output and eval-channel tasks before disposing xterm.
- A PTY `Exited` event marks that tab as no longer alive.
- Native window close and the app Quit menu both terminate registered PTYs.
