//! Multi tab terminal: one `portable-pty` session per tab, rendered by
//! `xterm.js` inside the webview.
//!
//! Threading model (this is the part that matters):
//!
//! * A reader thread per session pushes raw bytes into [`Outbox`], a
//!   `Send + Sync` queue, and wakes the pump through a `tokio::sync::Notify`.
//! * A writer thread per session drains [`PtyCommand`]s (input / resize / quit).
//! * Exactly one main thread task (the "pump") owns the webview handle: it
//!   drains the outbox, coalesces bursts and calls `evaluate_script`.
//! * Keyboard input, resize notifications and panel dragging travel the other
//!   way over a `dioxus` eval channel, so nothing polls.
//!
//! `wry::WebView` is neither `Send` nor `Sync`, so a PTY thread can never touch
//! it directly — routing everything through the pump is a correctness
//! requirement, not a style choice.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use dioxus_desktop::{use_window, DesktopContext};
use portable_pty::{Child, CommandBuilder, ExitStatus, NativePtySystem, PtySize, PtySystem};
use tokio::sync::{watch, Notify};

use crate::util::{base64_encode, escape_js_string, shell_label, strip_verbatim};

// ── tuning ───────────────────────────────────────────────────────────

/// Size of a single `read()` from the pty master.
const READ_BUF: usize = 16 * 1024;
/// How long the pump waits for more output before flushing what it has.
/// Bursts become a single `term.write()`, isolated keystrokes stay instant.
const FLUSH_WINDOW: Duration = Duration::from_millis(4);
/// Hard cap on a single batch, so a runaway `cat /dev/urandom` cannot grow an
/// unbounded JS string.
const MAX_BATCH_BYTES: usize = 256 * 1024;
/// Hard cap on everything waiting to be flushed. `MAX_BATCH_BYTES` only bounds
/// one flush, so without this a shell that outruns the pump (`yes`, a long
/// `cat /dev/urandom`) grows the queue until the process runs out of memory.
/// Oldest output is dropped first; exit events are never dropped.
const MAX_QUEUE_BYTES: usize = 4 * 1024 * 1024;

const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 30;
/// Panel height before the user drags the handle. Shared with the drag handler's
/// starting point so the webview and Rust cannot disagree about it.
pub const DEFAULT_PANEL_H: u32 = 340;
/// Panel chrome that is not usable by the terminal grid: the tab bar plus the
/// drag strip above it. The pane's title row was folded into the tab bar, so
/// there is no separate header any more.
const PANEL_CHROME: u32 = 38;
const MIN_PANEL_H: u32 = PANEL_CHROME + 80;
const MAX_PANEL_H: u32 = PANEL_CHROME + 700;
/// The editor keeps at least this much room, so dragging the split can never
/// squeeze the editor out of existence on a short window.
const MIN_EDITOR_H: u32 = 140;
/// How often the reap loops poll `try_wait`.
const REAP_STEP: Duration = Duration::from_millis(5);
/// How long the reader thread waits for the child's exit status after EOF.
const EXIT_REAP_WINDOW: Duration = Duration::from_millis(1500);

// ── PTY plumbing ─────────────────────────────────────────────────────

enum PtyCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
    Quit,
}

enum PtyEvent {
    Output(Vec<u8>),
    /// `None` when the status could not be collected within the reap window.
    Exited(Option<ExitStatus>),
}

impl PtyEvent {
    fn len(&self) -> usize {
        match self {
            PtyEvent::Output(bytes) => bytes.len(),
            PtyEvent::Exited(_) => 0,
        }
    }
}

/// `Send + Sync` hand-off point between the reader threads and the single
/// main thread pump. Using a queue plus a `Notify` instead of an async channel
/// keeps it `Clone`, which is what `use_hook` requires.
#[derive(Default)]
pub struct Outbox {
    queue: Mutex<Vec<(usize, PtyEvent)>>,
    bytes: AtomicUsize,
    truncated: Mutex<HashSet<usize>>,
    wake: Notify,
}

impl Outbox {
    fn push(&self, tab: usize, event: PtyEvent) {
        let size = event.len();
        if let Ok(mut queue) = self.queue.lock() {
            queue.push((tab, event));
            self.bytes.fetch_add(size, Ordering::Relaxed);
            if self.bytes.load(Ordering::Relaxed) > MAX_QUEUE_BYTES {
                let mut bytes = self.bytes.load(Ordering::Relaxed);
                // Oldest output goes first; `Exited` is tiny and load bearing, so
                // it is always kept.
                while bytes > MAX_QUEUE_BYTES {
                    match queue
                        .iter()
                        .position(|(_, e)| matches!(e, PtyEvent::Output(_)))
                    {
                        Some(index) => {
                            let (_, dropped) = queue.remove(index);
                            bytes = bytes.saturating_sub(dropped.len());
                        }
                        None => break,
                    }
                }
                self.bytes.store(bytes, Ordering::Relaxed);
                if let Ok(mut marked) = self.truncated.lock() {
                    marked.insert(tab);
                }
            }
        }
        self.wake.notify_one();
    }

    fn drain(&self) -> Vec<(usize, PtyEvent)> {
        let taken = match self.queue.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => Vec::new(),
        };
        if !taken.is_empty() {
            // Subtract what was actually taken rather than zeroing the counter:
            // a reader thread may have pushed in between.
            let drained: usize = taken.iter().map(|(_, event)| event.len()).sum();
            self.bytes.fetch_sub(drained, Ordering::Relaxed);
        }
        taken
    }

    /// Tabs whose output had to be dropped, so the pump can say so.
    fn take_truncated(&self) -> Vec<usize> {
        let Ok(mut marked) = self.truncated.lock() else {
            return Vec::new();
        };
        marked.drain().collect()
    }
}

type ChildHandle = Option<Box<dyn Child + Send + Sync>>;

/// One terminal tab's pseudo terminal.
///
/// `Send + Sync` on purpose: the app wide shutdown registry keeps weak
/// references to these and must be able to terminate them from a menu handler.
pub struct PtySession {
    id: usize,
    shell: String,
    cmd_tx: mpsc::Sender<PtyCommand>,
    /// Shared with the reader thread: that is the only place where the real
    /// exit status is known (the pty hits EOF there first), and `terminate`
    /// needs the same handle to kill the process.
    child: Arc<Mutex<ChildHandle>>,
    threads: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Shared with the reader thread so it can tell "the shell exited on its
    /// own" apart from "somebody tore this tab down".
    terminated: Arc<AtomicBool>,
}

impl PtySession {
    pub fn new(
        id: usize,
        cols: u16,
        rows: u16,
        out: Arc<Outbox>,
        cwd: Option<&Path>,
    ) -> Result<Arc<Self>, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PtyCommand>();
        let (cmd, shell) = shell_command(cwd);

        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {shell}: {e}"))?;

        // The slave end has to go away right after spawning. Keeping it open
        // would stop the reader from ever seeing EOF when the shell exits.
        drop(pair.slave);

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take_writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone_reader: {e}"))?;

        let mut threads = Vec::with_capacity(2);
        let child = Arc::new(Mutex::new(Some(child)));
        let terminated = Arc::new(AtomicBool::new(false));

        // Reader: pty -> outbox. Do not wait for a full buffer here: interactive
        // shells usually emit only a small prompt and may never fill READ_BUF.
        {
            let out = out.clone();
            let child = child.clone();
            let terminated = terminated.clone();
            threads.push(thread::spawn(move || {
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            out.push(id, PtyEvent::Output(buf[..n].to_vec()));
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                // The shell is gone; ask for its real status off this thread so
                // the UI never blocks on `try_wait`.
                let status = reap(&child, EXIT_REAP_WINDOW);
                // If somebody already tore the tab down there is nobody left to
                // tell, and reporting a made up status would only be noise.
                if terminated.load(Ordering::Relaxed) {
                    return;
                }
                out.push(id, PtyEvent::Exited(status));
            }));
        }

        // Writer: commands -> pty. Owns the master handle so that resizes work
        // and so that the last master reference disappears with the thread.
        {
            let master = pair.master;
            threads.push(thread::spawn(move || loop {
                match cmd_rx.recv() {
                    Ok(PtyCommand::Input(data)) => {
                        let _ = writer.write_all(&data);
                        let _ = writer.flush();
                    }
                    Ok(PtyCommand::Resize(cols, rows)) => {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    Ok(PtyCommand::Quit) | Err(_) => break,
                }
            }));
        }

        Ok(Arc::new(Self {
            id,
            shell,
            cmd_tx,
            child,
            threads: Mutex::new(threads),
            terminated,
        }))
    }

    pub fn send_input(&self, data: &[u8]) {
        if self.terminated.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.cmd_tx.send(PtyCommand::Input(data.to_vec()));
    }

    /// Changes directory after an explicit request from the user.
    pub fn change_cwd(&self, dir: &Path) -> bool {
        if self.terminated.load(Ordering::Relaxed) {
            return false;
        }
        if !dir.is_dir() {
            return false;
        }
        let Some(script) = cd_script(dir, &self.shell) else {
            return false;
        };
        // Deliberately not `send_input`: the shell must not consider this the
        // user's keystroke.
        self.cmd_tx
            .send(PtyCommand::Input(script.into_bytes()))
            .is_ok()
    }

    pub fn send_resize(&self, cols: u16, rows: u16) {
        if self.terminated.load(Ordering::Relaxed) {
            return;
        }
        if cols == 0 || rows == 0 {
            return;
        }
        let _ = self.cmd_tx.send(PtyCommand::Resize(cols, rows));
    }

    /// Kill the child and stop the pty. Idempotent, cheap and non blocking on
    /// the happy path, so the app wide shutdown path can call it while the
    /// component still holds its own `Arc`.
    pub fn terminate(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.cmd_tx.send(PtyCommand::Quit);
        // Kill first, release the lock, then collect the status: `reap` takes the
        // lock itself, so the guard has to be gone before calling it.
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
        reap(&self.child, Duration::from_millis(500));
        if let Ok(mut guard) = self.child.lock() {
            *guard = None;
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.terminate();
        if let Ok(mut threads) = self.threads.lock() {
            for handle in threads.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

/// Poll `try_wait` for at most `timeout`, returning the real exit status.
///
/// The lock is taken per poll and released again in between. Holding it for the
/// whole window (as this used to) meant `terminate` — which runs on the UI
/// thread when a tab closes — blocked for up to `EXIT_REAP_WINDOW`, freezing
/// the window whenever a tab was closed just after its shell exited. Releasing
/// between polls caps the wait at roughly `REAP_STEP`.
///
/// `None` on timeout or if `terminate` already took the handle, so a shell that
/// leaked the pty to a grandchild cannot hang anything.
fn reap(child: &Mutex<ChildHandle>, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let mut guard = child.lock().ok()?;
            let child = guard.as_mut()?;
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(REAP_STEP);
    }
}

/// Pick a shell. Windows Terminal users usually want `pwsh` when it exists,
/// otherwise `powershell`; everywhere else `$SHELL` wins.
///
/// `cwd` is the terminal's current directory, also used for new tabs.
fn shell_command(cwd: Option<&Path>) -> (CommandBuilder, String) {
    let program = shell_program();
    let mut cmd = CommandBuilder::new(&program);
    let dir = cwd
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    if let Some(dir) = dir {
        // `\\?\D:\...` is what `canonicalize` hands back, and a process start
        // wants a plain path: the verbatim form makes `lpCurrentDirectory`
        // reject the request or land somewhere else entirely.
        cmd.cwd(strip_verbatim(&dir));
    }
    // Tell the shell it is talking to a real 256 colour terminal.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let program_name = Path::new(&program)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if cfg!(windows) && matches!(program_name.as_str(), "pwsh" | "powershell") {
        // Preserve the user's prompt text, adding OSC 7 so xterm can report
        // shell `cd` commands back to the workspace without parsing screen text.
        cmd.arg("-NoLogo");
        cmd.arg("-NoExit");
        cmd.arg("-Command");
        // 彩色 Prompt：路径青色 + Git 分支洋红 + 箭头符号
        let ps_prompt = r#"
$global:__zmstop_prompt = (Get-Command prompt -CommandType Function -ErrorAction SilentlyContinue).ScriptBlock;
function global:prompt {
    $uri = [System.Uri]::new((Get-Location).Path).AbsoluteUri;
    [Console]::Write("$([char]27)]7;$uri$([char]7)");
    $loc = (Get-Location).Path;
    # 缩短路径显示
    if ($loc -eq $env:USERPROFILE) { $loc = "~" }
    elseif ($loc.StartsWith($env:USERPROFILE + "\")) { $loc = "~\" + $loc.Substring($env:USERPROFILE.Length + 1) }
    # Git 分支
    $git = "";
    try {
        $branch = git branch --show-current 2>$null;
        if ($branch) { $git = " $([char]27)[35m($branch)$([char]27)[0m"; }
    } catch {}
    # 彩色输出：青色路径 + Git分支 + 蓝色箭头
    Write-Host "$([char]27)[36m$loc$([char]27)[0m" -NoNewline;
    if ($git) { Write-Host $git -NoNewline; }
    Write-Host " $([char]27)[34m>$([char]27)[0m " -NoNewline;
    return " ";
}
# 启用文件列表彩色输出（兼容 PS5 和 PS7）
if ($PSVersionTable.PSVersion.Major -ge 7) {
    $PSStyle.FileInfo.Directory = "$([char]27)[34m";
    $PSStyle.FileInfo.SymbolicLink = "$([char]27)[36m";
    $PSStyle.FileInfo.Executable = "$([char]27)[32m";
} else {
    # PS5: 设置环境变量让 Get-ChildItem 显示彩色
    $env:TERM = 'xterm-256color';
}
"#;
        cmd.arg(ps_prompt);
    } else if cfg!(windows) && program_name == "cmd" {
        // cmd expands $P each time it draws a prompt; OSC 7 carries that path
        // back through xterm's parser while retaining the usual `$P>` prompt.
        cmd.env("PROMPT", "$E]7;file:///$P$E\\$P$G");
    } else if program_name == "bash" {
        let existing = std::env::var("PROMPT_COMMAND").unwrap_or_default();
        let report_cwd = r#"cwd=${PWD//%/%25}; cwd=${cwd// /%20}; cwd=${cwd//#/%23}; cwd=${cwd//\?/%3F}; printf '\033]7;file://%s\007' "$cwd""#;
        let prompt_command = if existing.is_empty() {
            report_cwd.to_owned()
        } else {
            format!("{existing}; {report_cwd}")
        };
        cmd.env("PROMPT_COMMAND", prompt_command);
    }
    (cmd, shell_label(&program).to_owned())
}

fn shell_program() -> String {
    if cfg!(windows) {
        ["pwsh.exe", "powershell.exe"]
            .into_iter()
            .find(|c| on_path(c))
            .map(str::to_owned)
            .or_else(|| std::env::var("COMSPEC").ok())
            .unwrap_or_else(|| "powershell.exe".to_owned())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned())
    }
}

/// The line sent to an idle shell to move it into `dir`.
///
/// Quoted for the shell we actually spawn: `Set-Location -LiteralPath` treats a
/// path as data rather than as an expression, which matters because the paths
/// here come from the file tree.
fn cd_script(dir: &Path, shell: &str) -> Option<String> {
    // Same reason as `cmd.cwd`: the shell must be given a plain path, or the
    // prefix it prints in every prompt is exactly the garbage the sidebar was
    // showing before.
    let dir = strip_verbatim(dir);
    let path = dir.to_str()?;
    if path.is_empty() {
        return None;
    }
    if cfg!(windows) {
        if shell.eq_ignore_ascii_case("cmd") {
            return Some(format!("cd /d \"{path}\"\r\n"));
        }
        // PowerShell escapes a single quote by doubling it inside '...'.
        Some(format!(
            "Set-Location -LiteralPath '{}'\r\n",
            path.replace('\'', "''")
        ))
    } else {
        // POSIX: close the quote, emit an escaped quote, reopen.
        Some(format!("cd -- '{}'\n", path.replace('\'', "'\\''")))
    }
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

// ── JavaScript snippets ──────────────────────────────────────────────
//
// Templates use `$NAME` placeholders instead of `format!` so the JS braces do
// not need escaping and the sources stay readable.

const XTERM_CSS: &str = include_str!("../assets/xterm.css");
const XTERM_JS: &str = include_str!("../assets/xterm.js");

static XTERM_CSS_B64: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static XTERM_JS_B64: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn theme_js(dark: bool) -> &'static str {
    if dark {
        // Dracula Pro 风格 - 鲜艳饱和的暗色主题
        r#"{background:'#0f111a',foreground:'#f8f8f2',cursor:'#8ab4ff',selectionBackground:'#44475a',black:'#21222c',red:'#ff5555',green:'#50fa7b',yellow:'#f1fa8c',blue:'#bd93f9',magenta:'#ff79c6',cyan:'#8be9fd',white:'#f8f8f2',brightBlack:'#6272a4',brightRed:'#ff6e6e',brightGreen:'#69ff94',brightYellow:'#ffffa5',brightBlue:'#d6acff',brightMagenta:'#ff92df',brightCyan:'#a4ffff',brightWhite:'#ffffff'}"#
    } else {
        // 柔和明亮的日间主题 - 高对比度彩色
        r#"{background:'#fafbfc',foreground:'#2d333b',cursor:'#4c6ef5',selectionBackground:'#c8e1ff',black:'#24292e',red:'#d73a49',green:'#28a745',yellow:'#dbab09',blue:'#0366d6',magenta:'#6f42c1',cyan:'#0598bc',white:'#959da5',brightBlack:'#586069',brightRed:'#cb2431',brightGreen:'#34d058',brightYellow:'#f9c513',brightBlue:'#2188ff',brightMagenta:'#8a63d2',brightCyan:'#39c5cf',brightWhite:'#2d333b'}"#
    }
}

/// Injected through `eval`, once per mount. Publishes the JS -> Rust channel
/// and installs the panel drag handler. Installed on `document` with event
/// delegation so it survives the panel being unmounted and remounted.
const BRIDGE_JS: &str = include_str!("../assets/terminal/bridge.js");

fn bridge_js() -> String {
    BRIDGE_JS
        .replace("$MIN_H", &MIN_PANEL_H.to_string())
        .replace("$MAX_H", &MAX_PANEL_H.to_string())
        .replace("$DEFAULT_H", &DEFAULT_PANEL_H.to_string())
        .replace("$MIN_EDITOR_H", &MIN_EDITOR_H.to_string())
}

/// Loads xterm's css/js exactly once. The 650 KB base64 payload lives in this
/// snippet only — it used to be re-formatted into every per-tab script.
fn xterm_boot_js() -> String {
    let css = XTERM_CSS_B64.get_or_init(|| base64_encode(XTERM_CSS.as_bytes()));
    let js = XTERM_JS_B64.get_or_init(|| base64_encode(XTERM_JS.as_bytes()));
    format!(
        r#"(function() {{
  if (!window.__maid_term) window.__maid_term = {{}};
  if (!document.getElementById('maid-xterm-css')) {{
    var st = document.createElement('style');
    st.id = 'maid-xterm-css';
    st.textContent = atob('{css}');
    document.head.appendChild(st);
  }}
  if (typeof Terminal === 'undefined') {{
    var sc = document.createElement('script');
    sc.id = 'maid-xterm-js';
    sc.textContent = atob('{js}');
    document.head.appendChild(sc);
  }}
}})();"#
    )
}

fn terminal_create_js(tab: usize, dark: bool, shell: &str) -> String {
    TEMPLATE_CREATE
        .replace("$TAB", &tab.to_string())
        .replace("$THEME", theme_js(dark))
        .replace("$SHELL", &escape_js_string(shell))
}

/// Builds (or rebuilds) the xterm instance for one tab.
///
/// Notes:
/// * The container may not be in the DOM yet: this runs from an effect which
///   fires before the edits reach the page, and a new tab's div only appears on
///   the next render. Hence the bounded retry.
/// * Everything user facing is posted straight to Rust, so there is no polling
///   loop anywhere in the pipeline.
const TEMPLATE_CREATE: &str = include_str!("../assets/terminal/create.js");

fn terminal_switch_js(tab: usize) -> String {
    TEMPLATE_SWITCH.replace("$TAB", &tab.to_string())
}

const TEMPLATE_SWITCH: &str = include_str!("../assets/terminal/switch.js");

fn terminal_dispose_js(tab: usize) -> String {
    TEMPLATE_DISPOSE.replace("$TAB", &tab.to_string())
}

const TEMPLATE_DISPOSE: &str = include_str!("../assets/terminal/dispose.js");

fn terminal_write_js(tab: usize, data: &[u8]) -> String {
    let encoded = base64_encode(data);
    let mut js = String::with_capacity(encoded.len() + 128);
    js.push_str("(function(){var e=window.__terminals&&window.__terminals[");
    js.push_str(&tab.to_string());
    js.push_str("];if(e&&e.term){var b=atob('");
    js.push_str(&encoded);
    js.push_str("');var u=new Uint8Array(b.length);for(var i=0;i<b.length;i++)u[i]=b.charCodeAt(i);e.term.write(u)}})()");
    js
}

/// Why a tab's shell is gone.
///
/// `ExitStatus` rather than a bare `i32` because it distinguishes "exited with
/// code 1" from "terminated by SIGTERM", which the int version collapsed into a
/// misleading number.
fn terminal_exited_js(tab: usize, status: Option<&ExitStatus>) -> String {
    let detail = match status {
        Some(status) => escape_js_string(&status.to_string()),
        None => "退出状态未知".to_owned(),
    };
    format!(
        "(function(){{var e=window.__terminals&&window.__terminals[{tab}];if(e){{e.term.write('\\r\\n\\x1b[33m[进程已退出：{detail}]\\x1b[0m\\r\\n')}}}})()"
    )
}

/// Said once when output had to be dropped to keep the queue bounded.
fn terminal_truncated_js(tab: usize) -> String {
    format!(
        "(function(){{var e=window.__terminals&&window.__terminals[{tab}];if(e){{e.term.write('\\r\\n\\x1b[31m[输出过快，已丢弃部分内容]\\x1b[0m\\r\\n')}}}})()"
    )
}

fn terminal_theme_js(dark: bool) -> String {
    TEMPLATE_THEME.replace("$THEME", theme_js(dark))
}

const TEMPLATE_THEME: &str = include_str!("../assets/terminal/theme.js");

/// Runs when the whole panel goes away.
const DISPOSE_ALL_JS: &str = include_str!("../assets/terminal/dispose_all.js");

/// Same teardown, but the JS → Rust channel survives.
///
/// Used by the pane's close button: the component stays mounted so a later
/// "show" can boot a fresh shell, and nulling `__maid_term.ch` here would leave
/// that new terminal unable to receive a single keystroke.
const DROP_TERMINALS_JS: &str = include_str!("../assets/terminal/drop_terminals.js");

fn console_error(desktop: &DesktopContext, message: &str) {
    let _ = desktop.webview.evaluate_script(&format!(
        "console.error('{}')",
        crate::util::escape_js_string(&format!("[Zmstop] {message}"))
    ));
}

fn show_terminal_error(
    desktop: &DesktopContext,
    tabs: &mut Signal<Vec<TabInfo>>,
    tab: usize,
    message: &str,
) {
    if let Ok(mut list) = tabs.try_write() {
        if let Some(info) = list.iter_mut().find(|info| info.id == tab) {
            info.alive = false;
            info.title = format!("终端 {} — 启动失败", tab + 1);
        }
    }
    let display_message = format!("终端启动失败：{message}");
    let escaped_message = crate::util::escape_js_string(&display_message);
    let script = format!(
        "(function(){{var e=document.getElementById('xterm-root-{tab}');if(e){{e.classList.add('terminal-error');e.textContent='{escaped_message}'}}}})()"
    );
    if let Err(error) = desktop.webview.evaluate_script(&script) {
        console_error(desktop, &format!("无法显示终端错误: {error}"));
    }
    console_error(
        desktop,
        &format!("终端 {tab} 初始化失败: {display_message}"),
    );
}

// ── tab state ────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct TabInfo {
    id: usize,
    title: String,
    alive: bool,
    cwd: Option<PathBuf>,
}

// ── Terminal component ───────────────────────────────────────────────

/// Multi tab terminal pane.
///
/// The pane stays mounted for the whole app lifetime: `visible` only controls
/// whether it is painted, so "minimise" keeps every shell, its scrollback and
/// any long running process exactly where they were. Bumping `close_request` is
/// the destructive path — it kills every session and arms a fresh boot for the
/// next time the pane is shown.
#[component]
pub fn Terminal(
    dark_mode: Signal<bool>,
    terminal_height: Signal<u32>,
    cwd: Signal<PathBuf>,
    directory_request: Signal<Option<PathBuf>>,
    visible: Signal<bool>,
    close_request: Signal<u64>,
) -> Element {
    let desktop = use_window();

    let mut tabs = use_signal(|| {
        vec![TabInfo {
            id: 0,
            title: "终端 1".to_owned(),
            alive: false,
            cwd: Some(cwd.peek().clone()),
        }]
    });
    let mut active = use_signal(|| 0usize);

    // `use_hook` hands out clones, so every piece of shared state has to be
    // behind an `Arc`. No raw pointers, no `unsafe`.
    let sessions: Arc<Mutex<Vec<Arc<PtySession>>>> = use_hook(|| Arc::new(Mutex::new(Vec::new())));
    let outbox: Arc<Outbox> = use_hook(|| Arc::new(Outbox::default()));
    let booted: Arc<AtomicBool> = use_hook(|| Arc::new(AtomicBool::new(false)));
    let next_id: Arc<AtomicUsize> = use_hook(|| Arc::new(AtomicUsize::new(1)));
    let shutdown = use_hook(|| watch::channel(false).0);

    // `eval` needs a component context, so the channel is opened once. Creating
    // it per render would leak one webview query per re-render.
    let bridge = use_hook(|| eval(&bridge_js()));

    // Both long running tasks are started from an effect: `spawn` is not
    // idempotent across renders, so doing it in the component body would fork a
    // second pump (and a second receiver) on every signal change.
    {
        let desktop = desktop.clone();
        let outbox = outbox.clone();
        let sessions = sessions.clone();
        let shutdown = shutdown.clone();
        let tabs = tabs;
        use_effect(move || {
            let mut pump_shutdown = shutdown.subscribe();
            let pump_desktop = desktop.clone();
            let pump_outbox = outbox.clone();
            let pump_tabs = tabs;
            spawn(async move {
                tokio::select! {
                    _ = pump_output(pump_desktop, pump_outbox, pump_tabs) => {}
                    _ = pump_shutdown.changed() => {}
                }
            });

            let mut event_shutdown = shutdown.subscribe();
            let event_sessions = sessions.clone();
            let event_desktop = desktop.clone();
            let event_tabs = tabs;
            spawn(async move {
                tokio::select! {
                    _ = forward_js_events(
                        bridge,
                        event_sessions,
                        terminal_height,
                        event_desktop,
                        event_tabs,
                        active,
                        cwd,
                    ) => {}
                    _ = event_shutdown.changed() => {}
                }
            });
        });
    }

    // ── boot: load xterm once, start the first tab ────────────────────
    //
    // `visible` is a dependency, so this is the single place that decides when a
    // shell exists: the first show boots it, a later show after a hard close
    // boots a fresh one, and a minimise in between does nothing at all.
    {
        let desktop = desktop.clone();
        let outbox = outbox.clone();
        let sessions = sessions.clone();
        let booted = booted.clone();
        use_effect(move || {
            if !visible() {
                return;
            }
            if booted.swap(true, Ordering::SeqCst) {
                return;
            }
            let _ = desktop.webview.evaluate_script(&xterm_boot_js());
            let start_dir = cwd();
            let start_dir = Some(start_dir.clone());
            let (_, shell) = shell_command(start_dir.as_deref());
            match PtySession::new(
                0,
                DEFAULT_COLS,
                DEFAULT_ROWS,
                outbox.clone(),
                start_dir.as_deref(),
            ) {
                Ok(session) => {
                    crate::cleanup::register(&session);
                    if let Ok(mut list) = sessions.lock() {
                        list.push(session);
                    }
                    if let Ok(mut list) = tabs.try_write() {
                        list[0].alive = true;
                        list[0].title = format!("终端 1 — {shell}");
                        list[0].cwd = start_dir.clone();
                    }
                    let _ = desktop.webview.evaluate_script(&terminal_create_js(
                        0,
                        *dark_mode.peek(),
                        &shell,
                    ));
                    let _ = desktop.webview.evaluate_script(&terminal_switch_js(0));
                }
                Err(e) => show_terminal_error(&desktop, &mut tabs, 0, &e),
            }
        });
    }

    // ── hard close: kill every session in this pane ───────────────────
    //
    // Reached from the pane's ✕ and from the menu. Everything is reset to the
    // pre-boot state so the next show starts one clean shell.
    {
        let desktop = desktop.clone();
        let sessions = sessions.clone();
        let booted = booted.clone();
        let next_id = next_id.clone();
        use_effect(move || {
            if close_request() == 0 {
                return;
            }
            // Hide before anything dies, so no output is painted on the way out.
            visible.set(false);
            let _ = desktop.webview.evaluate_script(DROP_TERMINALS_JS);
            // Terminate outside the lock: dropping a session joins its reader
            // and writer threads, and holding the lock across that would block
            // the pump for the length of two `EXIT_REAP_WINDOW`s.
            let taken = sessions
                .lock()
                .ok()
                .map(|mut list| std::mem::take(&mut *list));
            if let Some(list) = taken.as_ref() {
                for session in list {
                    session.terminate();
                }
            }
            drop(taken);

            booted.store(false, Ordering::SeqCst);
            next_id.store(1, Ordering::Relaxed);
            if let Ok(mut list) = tabs.try_write() {
                list.clear();
                list.push(TabInfo {
                    id: 0,
                    title: "终端 1".to_owned(),
                    alive: false,
                    cwd: Some(cwd.peek().clone()),
                });
            }
            if let Ok(mut slot) = active.try_write() {
                *slot = 0;
            }
        });
    }

    // A folder-picker request is an explicit user action, so it may change even
    // a shell that has already received input. Ordinary shell cwd updates never
    // cause this effect to run.
    {
        let sessions = sessions.clone();
        let desktop = desktop.clone();
        let mut tabs = tabs;
        let active = active;
        use_effect(move || {
            let Some(dir) = directory_request() else {
                return;
            };
            if !dir.is_dir() {
                console_error(
                    &desktop,
                    &format!("所选工作目录已不可用：{}", dir.display()),
                );
                directory_request.set(None);
                return;
            }
            let Some(tab_id) = tabs.read().get(*active.peek()).map(|tab| tab.id) else {
                return;
            };
            let session = sessions
                .lock()
                .ok()
                .and_then(|list| list.iter().find(|session| session.id == tab_id).cloned());
            if let Some(session) = session {
                session.change_cwd(&dir);
            }
            if let Ok(mut list) = tabs.try_write() {
                if let Some(tab) = list.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.cwd = Some(dir.clone());
                }
            }
            cwd.set(dir);
            directory_request.set(None);
        });
    }

    // ── theme sync ───────────────────────────────────────────────────
    {
        let desktop = desktop.clone();
        use_effect(move || {
            let dark = dark_mode();
            let _ = desktop.webview.evaluate_script(&terminal_theme_js(dark));
        });
    }

    // ── unmount ──────────────────────────────────────────────────────
    {
        let desktop = desktop.clone();
        let sessions = sessions.clone();
        let shutdown = shutdown.clone();
        use_drop(move || {
            shutdown.send_replace(true);
            // Tear the page down first so no in-flight output lands in a
            // terminal that is about to disappear.
            let _ = desktop.webview.evaluate_script(DISPOSE_ALL_JS);
            // Terminate and drop sessions outside the lock: dropping a session
            // joins its reader and writer threads.
            let taken = sessions
                .lock()
                .ok()
                .map(|mut list| std::mem::take(&mut *list));
            if let Some(sessions) = taken.as_ref() {
                for session in sessions {
                    session.terminate();
                }
            }
            drop(taken);
        });
    }

    // ── tab actions ──────────────────────────────────────────────────
    let add_tab = {
        let desktop = desktop.clone();
        let sessions = sessions.clone();
        let outbox = outbox.clone();
        let next_id = next_id.clone();
        move || {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let start_dir = cwd();
            let (_, shell) = shell_command(Some(start_dir.as_path()));
            match PtySession::new(
                id,
                DEFAULT_COLS,
                DEFAULT_ROWS,
                outbox.clone(),
                Some(start_dir.as_path()),
            ) {
                Ok(session) => {
                    crate::cleanup::register(&session);
                    if let Ok(mut list) = sessions.lock() {
                        list.push(session);
                    }
                    if let Ok(mut list) = tabs.try_write() {
                        list.push(TabInfo {
                            id,
                            title: format!("终端 {} — {shell}", id + 1),
                            alive: true,
                            cwd: Some(start_dir.clone()),
                        });
                        let idx = list.len() - 1;
                        if let Ok(mut a) = active.try_write() {
                            *a = idx;
                        }
                    }
                    let _ = desktop.webview.evaluate_script(&terminal_create_js(
                        id,
                        *dark_mode.peek(),
                        &shell,
                    ));
                    let _ = desktop.webview.evaluate_script(&terminal_switch_js(id));
                }
                Err(e) => console_error(&desktop, &e),
            }
        }
    };

    let close_tab = {
        let desktop = desktop.clone();
        let sessions = sessions.clone();
        move |idx: usize| {
            // Work out the new selection while the borrow of `tabs` is alive.
            let Some((tab_id, next_active)) = (|| {
                let Ok(mut list) = tabs.try_write() else {
                    return None;
                };
                if list.len() <= 1 {
                    return None; // always keep one tab
                }
                let tab_id = list.get(idx)?.id;
                list.remove(idx);
                // Closing a tab *before* the active one shifts it left; closing
                // the active one (or past the end) clamps.
                let cur = *active.peek();
                let next = if cur > idx {
                    cur - 1
                } else if cur >= list.len() {
                    list.len() - 1
                } else {
                    cur
                };
                Some((tab_id, next))
            })() else {
                return;
            };

            let _ = desktop
                .webview
                .evaluate_script(&terminal_dispose_js(tab_id));

            let removed = sessions.lock().ok().and_then(|mut list| {
                list.iter()
                    .position(|s| s.id == tab_id)
                    .map(|i| list.remove(i))
            });
            drop(removed);

            if let Ok(mut a) = active.try_write() {
                *a = next_active;
            }
            if let Ok(list) = tabs.try_read() {
                if let Some(tab) = list.get(next_active) {
                    if let Some(dir) = tab.cwd.clone() {
                        cwd.set(dir);
                    }
                    let _ = desktop.webview.evaluate_script(&terminal_switch_js(tab.id));
                }
            }
        }
    };

    let switch_tab = {
        let desktop = desktop.clone();
        move |idx: usize| {
            let Ok(list) = tabs.try_read() else { return };
            let Some(tab) = list.get(idx) else { return };
            if let Ok(mut a) = active.try_write() {
                *a = idx;
            }
            if let Some(dir) = tab.cwd.clone() {
                cwd.set(dir);
            }
            let _ = desktop.webview.evaluate_script(&terminal_switch_js(tab.id));
        }
    };

    // ── render ───────────────────────────────────────────────────────
    let current_tabs = tabs.read().clone();
    let active = *active.peek();
    let active_id = current_tabs.get(active).map(|t| t.id);

    rsx! {
        div { class: "terminal-tab-bar",
            // The tabs scroll; the pane controls stay pinned to the right edge,
            // which is why they sit outside the scrolling box.
            div { class: "terminal-tabs",
                for (i, tab) in current_tabs.iter().enumerate() {
                    // rsx attributes want expressions; a bare `"terminal-tab {x}"`
                    // string would be emitted with its braces still in it.
                    div {
                        class: tab_class(i == active, tab.alive),
                        title: tab.title.clone(),
                        onclick: {
                            let mut switch = switch_tab.clone();
                            move |_| switch(i)
                        },
                        span { class: "terminal-tab-title", {tab.title.clone()} }
                        if current_tabs.len() > 1 {
                            button {
                                class: "terminal-tab-close",
                                title: "关闭该终端标签",
                                onclick: {
                                    let mut close = close_tab.clone();
                                    move |e: Event<MouseData>| {
                                        e.stop_propagation();
                                        close(i);
                                    }
                                },
                                "×"
                            }
                        }
                    }
                }
                button {
                    class: "terminal-tab-add",
                    title: "新建终端",
                    onclick: {
                        let mut add = add_tab.clone();
                        move |_| add()
                    },
                    "+"
                }
            }
            // Two distinct actions, deliberately not sharing a glyph: minimise
            // hides the pane and leaves every shell running, close ends them.
            div { class: "terminal-pane-controls",
                button {
                    class: "terminal-pane-btn",
                    title: "最小化（会话保留）",
                    onclick: move |_| visible.set(false),
                    "▾"
                }
                button {
                    class: "terminal-pane-btn terminal-pane-close",
                    title: "关闭终端（结束全部会话）",
                    onclick: move |_| close_request += 1,
                    "✕"
                }
            }
        }
        div { class: "terminal-body",
            for tab in current_tabs.iter() {
                div {
                    key: "{tab.id}",
                    id: format!("xterm-root-{}", tab.id),
                    class: if Some(tab.id) == active_id {
                        "terminal-root active"
                    } else {
                        "terminal-root"
                    },
                }
            }
        }
    }
}

fn tab_class(active: bool, alive: bool) -> String {
    let mut class = String::from("terminal-tab");
    if active {
        class.push_str(" active");
    }
    if !alive {
        class.push_str(" dead");
    }
    class
}

// ── the single main thread pump ──────────────────────────────────────

/// The Rust side of the JS channel: keystrokes, `SIGWINCH`-style size updates
/// and panel drags. Nothing here polls — every event is a message.
async fn forward_js_events(
    mut bridge: UseEval,
    sessions: Arc<Mutex<Vec<Arc<PtySession>>>>,
    mut terminal_height: Signal<u32>,
    desktop: DesktopContext,
    mut tabs: Signal<Vec<TabInfo>>,
    active: Signal<usize>,
    mut cwd: Signal<PathBuf>,
) {
    loop {
        let msg = match bridge.recv().await {
            Ok(msg) => msg,
            Err(error) => {
                show_terminal_error(&desktop, &mut tabs, 0, &format!("桥接通道失败: {error:?}"));
                break;
            }
        };
        let kind = msg.get("t").and_then(|v| v.as_str()).unwrap_or("");
        let tab = msg
            .get("tab")
            .and_then(|v| v.as_u64())
            .unwrap_or(usize::MAX as u64) as usize;
        match kind {
            "error" => {
                let message = msg
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("终端 JavaScript 初始化失败");
                show_terminal_error(&desktop, &mut tabs, tab, message);
            }
            "in" => {
                let Some(data) = msg.get("d").and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Ok(list) = sessions.lock() {
                    if let Some(s) = list.iter().find(|s| s.id == tab) {
                        s.send_input(data.as_bytes());
                    }
                }
            }
            "size" => {
                let (Some(cols), Some(rows)) = (
                    msg.get("c").and_then(|v| v.as_u64()),
                    msg.get("r").and_then(|v| v.as_u64()),
                ) else {
                    continue;
                };
                if let Ok(list) = sessions.lock() {
                    if let Some(s) = list.iter().find(|s| s.id == tab) {
                        s.send_resize(cols as u16, rows as u16);
                    }
                }
            }
            "cwd" => {
                let Some(path) = msg.get("d").and_then(|v| v.as_str()).map(PathBuf::from) else {
                    continue;
                };
                let checked_path = path.clone();
                match tokio::task::spawn_blocking(move || checked_path.is_dir()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        console_error(
                            &desktop,
                            &format!("终端报告的工作目录不可用：{}", path.display()),
                        );
                        continue;
                    }
                    Err(error) => {
                        console_error(&desktop, &format!("检查终端工作目录失败：{error}"));
                        continue;
                    }
                }
                let Ok(mut list) = tabs.try_write() else {
                    continue;
                };
                let Some(info) = list.iter_mut().find(|info| info.id == tab) else {
                    continue;
                };
                info.cwd = Some(path.clone());
                if list.get(*active.peek()).map(|info| info.id) == Some(tab) {
                    cwd.set(path);
                }
            }
            "h" => {
                let Some(v) = msg.get("v").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let v = (v as u32).clamp(MIN_PANEL_H, MAX_PANEL_H);
                // `try_write` instead of `write`: a message can land just as the
                // panel unmounts, and panicking on a dead signal would take the
                // whole app down.
                if let Ok(mut h) = terminal_height.try_write() {
                    if *h != v {
                        *h = v;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Owns the webview handle for the lifetime of the panel. Every byte the shell
/// prints goes through here, and therefore so does every `evaluate_script`.
async fn pump_output(desktop: DesktopContext, outbox: Arc<Outbox>, mut tabs: Signal<Vec<TabInfo>>) {
    loop {
        outbox.wake.notified().await;
        let mut batch = outbox.drain();
        if batch.is_empty() {
            continue; // stale wakeup
        }

        let mut chunks: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut exits: Vec<(usize, Option<ExitStatus>)> = Vec::new();
        let mut total = 0usize;
        let deadline = Instant::now() + FLUSH_WINDOW;
        loop {
            for (tab, event) in batch.drain(..) {
                match event {
                    PtyEvent::Output(bytes) => {
                        total += bytes.len();
                        chunks.push((tab, bytes));
                    }
                    PtyEvent::Exited(status) => exits.push((tab, status)),
                }
            }

            if total >= MAX_BATCH_BYTES || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
            batch = outbox.drain();
            if batch.is_empty() {
                break; // burst is over, flush what we have
            }
        }

        // One `term.write()` per tab per flush: adjacent chunks are concatenated
        // so a burst of small reads does not become a burst of evals.
        let mut pending: HashMap<usize, Vec<u8>> = HashMap::new();
        for (tab, bytes) in chunks {
            pending.entry(tab).or_default().extend_from_slice(&bytes);
        }

        for (tab, bytes) in pending {
            let _ = desktop
                .webview
                .evaluate_script(&terminal_write_js(tab, &bytes));
        }

        for tab in outbox.take_truncated() {
            let _ = desktop.webview.evaluate_script(&terminal_truncated_js(tab));
        }

        for (tab, status) in exits {
            let _ = desktop
                .webview
                .evaluate_script(&terminal_exited_js(tab, status.as_ref()));
            if let Ok(mut list) = tabs.try_write() {
                if let Some(info) = list.iter_mut().find(|info| info.id == tab) {
                    info.alive = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_round_trips_shell_output() {
        let outbox = Arc::new(Outbox::default());
        let session = PtySession::new(0, DEFAULT_COLS, DEFAULT_ROWS, outbox.clone(), None)
            .expect("the selected shell should start in a PTY");
        let (_, shell) = shell_command(None);
        let command: &[u8] = if cfg!(windows) {
            if shell.eq_ignore_ascii_case("cmd") {
                b"set A=PTY_\r\nset B=OUTPUT_MARKER\r\necho %A%%B%\r\n"
            } else {
                b"Write-Output ('PTY_' + 'OUTPUT_' + 'MARKER')\r\n"
            }
        } else {
            b"printf '%s%s%s\\n' 'PTY_' 'OUTPUT_' 'MARKER'\n"
        };
        session.send_input(command);

        let marker = b"PTY_OUTPUT_MARKER";
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut received = Vec::new();
        let mut found = false;
        while Instant::now() < deadline {
            for (_, event) in outbox.drain() {
                if let PtyEvent::Output(bytes) = event {
                    received.extend_from_slice(&bytes);
                    if received.windows(marker.len()).any(|part| part == marker) {
                        found = true;
                        break;
                    }
                }
            }
            if found {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        session.terminate();

        assert!(
            found,
            "shell output did not reach the PTY reader: {}",
            String::from_utf8_lossy(&received)
        );
    }

    /// Collects everything the pty emits until `marker` shows up or time runs out.
    fn read_until(outbox: &Outbox, marker: &[u8], seconds: u64) -> (bool, String) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut received = Vec::new();
        while Instant::now() < deadline {
            for (_, event) in outbox.drain() {
                if let PtyEvent::Output(bytes) = event {
                    received.extend_from_slice(&bytes);
                    if received.windows(marker.len()).any(|part| part == marker) {
                        return (true, String::from_utf8_lossy(&received).into_owned());
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        (false, String::from_utf8_lossy(&received).into_owned())
    }

    /// Prints the shell's working directory, in whatever dialect it speaks.
    fn print_cwd_command(shell: &str) -> &'static [u8] {
        if cfg!(windows) {
            if shell.eq_ignore_ascii_case("cmd") {
                b"cd\r\n"
            } else {
                b"Write-Output (Get-Location).Path\r\n"
            }
        } else {
            b"pwd\n"
        }
    }

    #[test]
    fn shell_starts_and_moves_with_the_terminal_directory() {
        // A directory that exists, so `shell_command` has something to aim at.
        let dir = std::env::temp_dir().join("desktop-pty-cwd-test");
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        let want = std::fs::canonicalize(&dir).expect("temp dir should resolve");
        let nested = want.join("nested");
        std::fs::create_dir_all(&nested).expect("nested dir should be creatable");
        let nested = std::fs::canonicalize(&nested).expect("nested dir should resolve");
        let (_, shell) = shell_command(None);

        let reports = |dir: &Path| {
            // The shell is handed a plain path (see `cd_script`), so a plain
            // path is what comes back.
            let needle = strip_verbatim(dir).to_string_lossy().into_owned();
            move |outbox: &Outbox| {
                let (exact, seen) = read_until(outbox, needle.as_bytes(), 8);
                // Some shells print an 8.3 short path; compare case folded so a
                // drive letter case difference is not a failure.
                exact || seen.to_lowercase().contains(&needle.to_lowercase())
            }
        };

        // The shell starts at the terminal's current directory.
        let outbox = Arc::new(Outbox::default());
        let session = PtySession::new(0, DEFAULT_COLS, DEFAULT_ROWS, outbox.clone(), Some(&want))
            .expect("the selected shell should start in a PTY");
        session.send_input(print_cwd_command(&shell));
        assert!(
            reports(&want)(&outbox),
            "shell did not start in {}",
            want.display()
        );
        // An explicit directory-picker action may move an already-used shell.
        assert!(
            session.change_cwd(&nested),
            "explicit cwd change should succeed"
        );
        session.send_input(print_cwd_command(&shell));
        assert!(
            reports(&nested)(&outbox),
            "explicit cwd change did not move the shell to {}",
            nested.display()
        );
        session.terminate();

        // A path that is not a directory is never sent to a shell.
        let outbox = Arc::new(Outbox::default());
        let session = PtySession::new(0, DEFAULT_COLS, DEFAULT_ROWS, outbox.clone(), None)
            .expect("the selected shell should start in a PTY");
        assert!(
            !session.change_cwd(&want.join("no-such-dir")),
            "missing dir"
        );
        assert!(!session.change_cwd(Path::new("")), "empty path");
        session.terminate();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supported_shell_prompts_emit_osc7_directory_reports() {
        let program = shell_program().to_ascii_lowercase();
        if !cfg!(windows) && !program.ends_with("bash") {
            return;
        }
        let outbox = Arc::new(Outbox::default());
        let session = PtySession::new(0, DEFAULT_COLS, DEFAULT_ROWS, outbox.clone(), None)
            .expect("the selected shell should start in a PTY");
        let (reported, output) = read_until(&outbox, b"\x1b]7;", 8);
        session.terminate();
        assert!(
            reported,
            "shell prompt did not emit OSC 7: {}",
            output.escape_debug()
        );
    }

    #[test]
    fn cd_script_quotes_paths_for_the_shell() {
        let (_, shell) = shell_command(None);
        if cfg!(windows) {
            if shell.eq_ignore_ascii_case("cmd") {
                assert_eq!(
                    cd_script(Path::new(r"D:\a b\c'd"), &shell).as_deref(),
                    Some("cd /d \"D:\\a b\\c'd\"\r\n")
                );
                assert_eq!(
                    cd_script(Path::new(r"\\?\D:\a b"), &shell).as_deref(),
                    Some("cd /d \"D:\\a b\"\r\n")
                );
            } else {
                assert_eq!(
                    cd_script(Path::new(r"D:\a b\c'd"), &shell).as_deref(),
                    Some("Set-Location -LiteralPath 'D:\\a b\\c''d'\r\n")
                );
                // The verbatim prefix `canonicalize` produces never reaches a shell,
                // or every prompt would print it back.
                assert_eq!(
                    cd_script(Path::new(r"\\?\D:\a b"), &shell).as_deref(),
                    Some("Set-Location -LiteralPath 'D:\\a b'\r\n")
                );
            }
        } else {
            assert_eq!(
                cd_script(Path::new("/tmp/a b/c'd"), &shell).as_deref(),
                Some("cd -- '/tmp/a b/c'\\''d'\n")
            );
        }
        assert_eq!(cd_script(Path::new(""), &shell), None);
    }

    #[test]
    fn cmd_directory_script_uses_drive_switch() {
        if cfg!(windows) {
            assert_eq!(
                cd_script(Path::new(r"E:\work tree"), "cmd").as_deref(),
                Some("cd /d \"E:\\work tree\"\r\n")
            );
        }
    }
}