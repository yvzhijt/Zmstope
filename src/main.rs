#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![allow(dependency_on_unit_never_type_fallback)]

mod editor;
mod filetree;
mod icons;
mod pty;
mod util;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use dioxus::desktop::muda::accelerator::Accelerator;
use dioxus::desktop::muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::tao::window::Icon;
use dioxus::desktop::{
    use_muda_event_handler, use_window, use_wry_event_handler, Config, DesktopContext, LogicalSize,
    WindowBuilder, WindowCloseBehaviour, WindowEvent,
};
use dioxus::prelude::*;

use editor::{check_openable, editor_zoom_js, read_file_to_string, EditorPanel, EditorTab};
use filetree::FileTree;
use pty::Terminal;
use util::{base64_encode, escape_js_string, strip_verbatim};

// ── PTY cleanup registry ────────────────────────────────────────────

pub(crate) mod cleanup {
    use crate::pty::PtySession;
    use std::sync::{Arc, Mutex, Weak};

    static REGISTRY: Mutex<Vec<Weak<PtySession>>> = Mutex::new(Vec::new());

    pub fn register(session: &Arc<PtySession>) {
        let Ok(mut list) = REGISTRY.lock() else {
            return;
        };
        // Prune dead entries here. Without this the registry grows by one slot per
        // terminal tab ever created for the whole life of the process.
        list.retain(|weak| weak.strong_count() > 0);
        list.push(Arc::downgrade(session));
    }

    pub fn close_all() {
        let Ok(mut list) = REGISTRY.lock() else {
            return;
        };
        for weak in list.drain(..) {
            if let Some(session) = weak.upgrade() {
                session.terminate();
            }
        }
    }
}

// ── Assets ──────────────────────────────────────────────────────────

const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon-64.png");
const APP_CSS: &str = include_str!("../assets/app.css");
const TERMINAL_CSS: &str = include_str!("../assets/terminal.css");

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const FONT_JBM_REGULAR: &[u8] = include_bytes!("../fonts/webfonts/JetBrainsMono-Regular.woff2");
const FONT_JBM_BOLD: &[u8] = include_bytes!("../fonts/webfonts/JetBrainsMono-Bold.woff2");
const FONT_JBM_ITALIC: &[u8] = include_bytes!("../fonts/webfonts/JetBrainsMono-Italic.woff2");
const FONT_JBM_BOLD_ITALIC: &[u8] = include_bytes!("../fonts/webfonts/JetBrainsMono-BoldItalic.woff2");

static FONT_FACE_CSS: OnceLock<String> = OnceLock::new();

fn font_face_css() -> &'static str {
    FONT_FACE_CSS.get_or_init(|| {
        format!(
            "@font-face{{font-family:'JetBrains Mono';src:url(data:font/woff2;base64,{r})format('woff2');font-weight:400;font-style:normal}}\
             @font-face{{font-family:'JetBrains Mono';src:url(data:font/woff2;base64,{b})format('woff2');font-weight:700;font-style:normal}}\
             @font-face{{font-family:'JetBrains Mono';src:url(data:font/woff2;base64,{i})format('woff2');font-weight:400;font-style:italic}}\
             @font-face{{font-family:'JetBrains Mono';src:url(data:font/woff2;base64,{bi})format('woff2');font-weight:700;font-style:italic}}",
            r = base64_encode(FONT_JBM_REGULAR),
            b = base64_encode(FONT_JBM_BOLD),
            i = base64_encode(FONT_JBM_ITALIC),
            bi = base64_encode(FONT_JBM_BOLD_ITALIC),
        )
    })
}

static APP_ICON: OnceLock<Icon> = OnceLock::new();

fn app_icon() -> &'static Icon {
    APP_ICON.get_or_init(|| {
        let img = image::load_from_memory(APP_ICON_BYTES).expect("failed to load app icon");
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), w, h).expect("failed to create window icon")
    })
}

// ── Menu ────────────────────────────────────────────────────────────

/// Mirror an error into the webview console.
///
/// The binary is built with `windows_subsystem = "windows"`, so `eprintln!` has
/// nowhere to go and failures are silently dropped. This is the terminal module's
/// `console_error` promoted to app-shell level.
fn log_error(desktop: &DesktopContext, message: &str) {
    let _ = desktop.webview.evaluate_script(&format!(
        "console.error('{}')",
        escape_js_string(&format!("[Zmstop] {message}"))
    ));
}

/// How long a toast stays on screen.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);
static TOAST_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Show `message` as a toast and mirror it to the console, then clear it.
///
/// Each new notification invalidates older timers, so a burst of messages
/// neither clears the latest banner early nor leaves it visible indefinitely.
pub(crate) fn notify(desktop: DesktopContext, notice: Signal<String>, message: String) {
    let _ = desktop.webview.evaluate_script(&format!(
        "console.info('{}')",
        escape_js_string(&format!("[Zmstop] {message}"))
    ));
    show_notice(notice, message);
}

pub(crate) fn notify_error(desktop: DesktopContext, notice: Signal<String>, message: String) {
    log_error(&desktop, &message);
    show_notice(notice, message);
}

fn show_notice(mut notice: Signal<String>, message: String) {
    let generation = TOAST_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let mut timer = notice;
    // Notifications can be raised by EditorPanel. A regular `spawn` belongs
    // to that component and is cancelled when save-and-close unmounts it,
    // leaving the app-level toast visible forever. Keep the timer at root
    // scope so its lifetime matches the notice signal instead.
    spawn_forever(async move {
        tokio::time::sleep(TOAST_TTL).await;
        if TOAST_GENERATION.load(Ordering::Relaxed) == generation {
            timer.set(String::new());
        }
    })
    .expect("notification timer requires an active Dioxus runtime");
    notice.set(message);
}

/// Where the file tree points before the user has picked a workspace.
fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Last segment of a workspace path, for the header. Falls back to the whole
/// path for a filesystem root such as `C:\`, which has no final component.
fn workspace_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Asks for a folder and returns it as the next workspace root, or `None` if
/// the user cancelled.
///
/// Shared by the header button and the 文件 menu so both get the same
/// canonicalisation and the same error reporting. `rfd` is modal and pumps its
/// own message loop, so calling it inline is fine and keeps the result on the
/// render thread — a `Signal` is not `Send`, so this deliberately avoids
/// handing the work to a thread.
fn choose_workspace(
    desktop: &DesktopContext,
    start: &Path,
    notice: Signal<String>,
) -> Option<PathBuf> {
    let picked = rfd::FileDialog::new()
        .set_directory(start)
        .set_title("选择工作区文件夹")
        .pick_folder()?;
    // Canonicalise so the header, the tree and the shell all name the folder
    // identically, and so nothing downstream has to cope with a relative or
    // `..`-containing path.
    Some(match std::fs::canonicalize(&picked) {
        Ok(full) => full,
        Err(_) => {
            notify_error(
                desktop.clone(),
                notice,
                format!("无法解析该路径：{}", picked.display()),
            );
            picked
        }
    })
}

fn acc(s: &str) -> Option<Accelerator> {
    s.parse().ok()
}

/// Nudges the editor font size from the 查看 menu. A no-op until the editor has
/// mounted, which is harmless: the zoom level lives in the bundle, so the first
/// view to be created already starts at whatever size was last chosen.
fn zoom_editor(desktop: &DesktopContext, delta: i32) {
    let _ = desktop.webview.evaluate_script(&editor_zoom_js(delta));
}

/// The menu keys, installed once for the life of the window.
///
/// muda attaches a real accelerator to every item in `create_menu`, but on
/// Windows a menu accelerator only fires when the message loop calls
/// `TranslateAcceleratorW`, and neither tao 0.26 nor dioxus-desktop 0.5 ever
/// does — so `Ctrl+O` / `Ctrl+Q` / `` Ctrl+` `` / `Ctrl+Shift+T` were purely
/// decorative and the items were reachable only by mouse. This is the missing
/// half: it posts the menu's own ids, so the keyboard route cannot drift away
/// from the menu one.
///
/// Deliberately *not* bound here:
/// * `Ctrl+= / Ctrl+- / Ctrl+0` — the editor owns its own zoom keys, and they
///   are already wired through `查看` from the Rust side.
/// * `Ctrl+S` — bound in `editor.rs` so it can stay clear of the terminal.
/// * `Ctrl+Q` inside the terminal — bash uses it to resume output after
///   `Ctrl+S`, and taking it would strand the shell.
const SHORTCUT_JS: &str = r#"
(function () {
  if (window.__maid_keys) return;
  window.__maid_keys = true;
  var ch = dioxus;
  window.__maid_cmd = function (id) {
    try { ch.send({ t: 'cmd', id: id }); } catch (e) {}
  };

  window.addEventListener('keydown', function (ev) {
    if (!(ev.ctrlKey || ev.metaKey) || ev.altKey) return;

    var id = null;
    if (ev.shiftKey) {
      if (ev.key.toLowerCase() === 't') id = 'theme';
      else return;
    } else {
      switch (ev.key) {
        case 'o': case 'O': id = 'open-folder'; break;
        case '`': case '~': id = 'terminal'; break;
        case 'q': case 'Q':
          if (ev.target && typeof ev.target.closest === 'function' &&
              ev.target.closest('.terminal-root')) return;
          id = 'quit';
          break;
        default: return;
      }
    }

    ev.preventDefault();
    ev.stopPropagation();
    if (!ev.repeat) window.__maid_cmd(id);
  }, true);
})();
"#;

const WORKSPACE_SPLITTER_JS: &str = r#"
(function () {
  if (window.__maid_workspace_splitter) return;
  var width = 240;
  var dragging = false;
  var startX = 0;
  var startWidth = width;
  var ch = dioxus;
  var sendWidth = function (value) {
    var max = Math.min(420, Math.max(180, window.innerWidth - 500));
    width = Math.round(Math.max(180, Math.min(max, value)));
    try { ch.send({ t: 'workspace-width', v: width }); } catch (e) {}
  };
  window.__maid_workspace_splitter = true;

  document.addEventListener('pointerdown', function (ev) {
    var handle = ev.target && ev.target.closest
      ? ev.target.closest('.workspace-resize-handle') : null;
    if (!handle) return;
    dragging = true;
    startX = ev.clientX;
    startWidth = width;
    document.body.classList.add('workspace-dragging');
    ev.preventDefault();
  }, true);
  document.addEventListener('pointermove', function (ev) {
    if (!dragging) return;
    sendWidth(startWidth + ev.clientX - startX);
    ev.preventDefault();
  }, { passive: false });
  function stopDragging() {
    if (!dragging) return;
    dragging = false;
    document.body.classList.remove('workspace-dragging');
  }
  document.addEventListener('pointerup', stopDragging, true);
  document.addEventListener('pointercancel', stopDragging, true);
  document.addEventListener('keydown', function (ev) {
    var handle = ev.target && ev.target.closest
      ? ev.target.closest('.workspace-resize-handle') : null;
    if (!handle || (ev.key !== 'ArrowLeft' && ev.key !== 'ArrowRight')) return;
    sendWidth(width + (ev.key === 'ArrowRight' ? 12 : -12));
    ev.preventDefault();
  }, true);
})();
"#;

/// Every menu command, so the menu and the key bridge share one implementation.
///
/// `Clone` rather than shared: muda's handler and the bridge's receive loop are
/// two independent `FnMut`s, and each needs its own owned copy.
#[derive(Clone)]
struct Commands {
    desktop: DesktopContext,
    dark_mode: Signal<bool>,
    terminal_visible: Signal<bool>,
    terminal_close: Signal<u64>,
    terminal_cwd: Signal<PathBuf>,
    directory_request: Signal<Option<PathBuf>>,
    about_open: Signal<bool>,
    notice: Signal<String>,
}

impl Commands {
    /// Returns `false` for ids it does not own — notably `save`, which belongs
    /// to the editor panel and is handled in `editor.rs`.
    fn run(&mut self, id: &str) -> bool {
        match id {
            "quit" => {
                cleanup::close_all();
                self.desktop.close();
            }
            "theme" => self.dark_mode.toggle(),
            "open-folder" => {
                let start = self.terminal_cwd.peek().clone();
                if let Some(dir) = choose_workspace(&self.desktop, &start, self.notice) {
                    self.directory_request.set(Some(dir));
                }
            }
            "font-bigger" => zoom_editor(&self.desktop, 1),
            "font-smaller" => zoom_editor(&self.desktop, -1),
            "font-reset" => zoom_editor(&self.desktop, 0),
            "terminal" => {
                self.terminal_visible.toggle();
            }
            "terminal-close" => self.terminal_close += 1,
            "about" | "about_help" => self.about_open.set(true),
            _ => return false,
        }
        true
    }
}

fn create_menu() -> Menu {
    let menu = Menu::new();

    let file_menu = Submenu::new("文件", true);
    file_menu
        .append_items(&[
            &MenuItem::with_id("save", "保存", true, acc("CmdOrCtrl+S")),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("open-folder", "打开文件夹…", true, acc("CmdOrCtrl+O")),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("about", "关于桌面", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("quit", "退出", true, acc("CmdOrCtrl+Q")),
        ])
        .unwrap();

    let view_menu = Submenu::new("查看", true);
    view_menu
        .append_items(&[
            &MenuItem::with_id(
                "terminal",
                "显示 / 隐藏终端",
                true,
                acc("CmdOrCtrl+Backquote"),
            ),
            &MenuItem::with_id("terminal-close", "关闭终端（结束会话）", true, None),
            &PredefinedMenuItem::separator(),
            // No accelerators on purpose: Windows translates menu accelerators
            // before the webview is offered the key, so claiming Ctrl+= / Ctrl+-
            // here would stop the editor's own zoom binding from ever firing.
            &MenuItem::with_id("font-bigger", "放大编辑器字号 (Ctrl +)", true, None),
            &MenuItem::with_id("font-smaller", "缩小编辑器字号 (Ctrl -)", true, None),
            &MenuItem::with_id("font-reset", "重置编辑器字号 (Ctrl 0)", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(
                "theme",
                "切换主题 (日间/夜间)",
                true,
                acc("CmdOrCtrl+Shift+T"),
            ),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(Some("全屏")),
        ])
        .unwrap();

    let window_menu = Submenu::new("窗口", true);
    window_menu
        .append_items(&[
            &PredefinedMenuItem::minimize(Some("最小化")),
            &PredefinedMenuItem::maximize(Some("最大化")),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::close_window(Some("关闭窗口")),
        ])
        .unwrap();

    let help_menu = Submenu::new("帮助", true);
    help_menu
        .append_items(&[&MenuItem::with_id("about_help", "关于Zmstop", true, None)])
        .unwrap();

    menu.append_items(&[&file_menu, &view_menu, &window_menu, &help_menu])
        .unwrap();

    menu
}

fn main() {
    let menu = create_menu();

    let window = WindowBuilder::new()
        .with_title("Zmstop")
        .with_window_icon(Some(app_icon().clone()))
        .with_resizable(true)
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_min_inner_size(LogicalSize::new(720.0, 480.0));

    let config = Config::new()
        .with_menu(menu)
        .with_window(window)
        .with_close_behaviour(WindowCloseBehaviour::LastWindowExitsApp);

    dioxus::desktop::launch::launch(App, vec![], config);
}

// ── App ─────────────────────────────────────────────────────────────

#[component]
fn App() -> Element {
    let desktop = use_window();
    // Two private copies of `desktop`, taken while it is still the local `let`.
    // `Commands` takes ownership of the original for the menu and key bridges.
    let error_reporter = desktop.clone();
    // The 打开文件夹 button needs one too, for the same reason.
    let dialog_reporter = desktop.clone();

    let dark_mode = use_signal(|| false);
    // The pane is open from the first frame: the terminal is the entry point of
    // this app, not something to summon from a menu. The `Terminal` component's
    // boot effect keys off this signal, so a `true` here is what actually starts
    // a shell.
    //
    // `terminal_visible` only decides whether the pane is painted: the `Terminal`
    // component stays mounted so hiding it keeps every shell, its scrollback and
    // any running process. `terminal_close` is the destructive request that
    // actually ends the sessions.
    let mut terminal_visible = use_signal(|| true);
    let terminal_close = use_signal(|| 0u64);
    let mut about_open = use_signal(|| false);
    let terminal_height = use_signal(|| pty::DEFAULT_PANEL_H);
    let mut notice = use_signal(String::new);
    let file_tree_width = use_signal(|| 240u32);

    // Editor state
    let editor_tabs = use_signal(Vec::<EditorTab>::new);
    let active_editor_tab = use_signal(|| 0usize);
    // The active shell is the source of truth for the workspace directory.
    // File-tree navigation and editor tabs are views/actions within that cwd,
    // never a competing source that can move the terminal behind the user's back.
    let terminal_cwd = use_signal(default_root);
    let mut directory_request = use_signal(|| None::<PathBuf>);

    use_wry_event_handler(move |event, _| {
        if matches!(
            event,
            TaoEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
        ) {
            cleanup::close_all();
        }
    });

    // One implementation of every menu command, shared by the menu below and by
    // the key bridge right after it.
    let commands = Commands {
        desktop,
        dark_mode,
        terminal_visible,
        terminal_close,
        terminal_cwd,
        directory_request,
        about_open,
        notice,
    };

    use_muda_event_handler({
        let mut commands = commands.clone();
        move |event| {
            commands.run(event.id.0.as_str());
        }
    });

    // The keyboard half of the menu. Started from an effect because `spawn` is
    // not idempotent across renders; ends when the eval channel dies with the
    // scope.
    {
        let bridge = use_hook(|| eval(SHORTCUT_JS));
        use_effect(move || {
            let mut bridge = bridge;
            let mut commands = commands.clone();
            spawn(async move {
                while let Ok(message) = bridge.recv().await {
                    if message.get("t").and_then(|v| v.as_str()) != Some("cmd") {
                        continue;
                    }
                    let Some(id) = message.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    commands.run(id);
                }
            });
        });
    }

    {
        let bridge = use_hook(|| eval(WORKSPACE_SPLITTER_JS));
        use_effect(move || {
            let mut bridge = bridge;
            let mut width = file_tree_width;
            spawn(async move {
                while let Ok(message) = bridge.recv().await {
                    if message.get("t").and_then(|v| v.as_str()) != Some("workspace-width") {
                        continue;
                    }
                    let Some(value) = message.get("v").and_then(|v| v.as_u64()) else {
                        continue;
                    };
                    let value = (value as u32).clamp(180, 420);
                    if *width.peek() != value {
                        width.set(value);
                    }
                }
            });
        });
    }

    // One read, reused by the render below: an open tab is what puts an
    // `EditorPanel` above the terminal, and therefore what makes the pane a
    // bottom dock instead of the whole column.
    let editor_open = !editor_tabs.read().is_empty();
    let panel_style = if editor_open {
        format!("height: {}px", terminal_height())
    } else {
        // No `EditorPanel` in the column, so the pane *is* the column. A docked
        // height here would leave it floating at the top of the main area with
        // the background showing underneath instead of filling it.
        String::new()
    };
    let about_version = format!("版本 {APP_VERSION}");

    // Header text for the workspace. Kept out of the `rsx!` because a `match`
    // in element-children position does not parse there.
    let current_dir = terminal_cwd();
    let ws_name = workspace_label(&current_dir);
    let ws_title = strip_verbatim(&current_dir).display().to_string();

    let mut root_class = String::from("app-root");
    if dark_mode() {
        root_class.push_str(" dark");
    }

    // Open file handler
    let open_file = {
        let tabs = editor_tabs;
        let mut active = active_editor_tab;
        let notify_desktop = error_reporter.clone();
        let notice = notice;
        move |path: PathBuf| {
            // Already open: just focus it. The text is *not* reloaded, otherwise
            // unsaved edits in that tab would be thrown away by a stray click.
            let tabs_read = tabs.read();
            if let Some(idx) = tabs_read.iter().position(|t| t.path == path) {
                drop(tabs_read);
                if let Ok(mut a) = active.try_write() {
                    *a = idx;
                }
                return;
            }
            drop(tabs_read);

            // Both the metadata preflight and content read run on Tokio's
            // blocking pool. `dioxus::spawn`
            // would not have helped: its futures run on the VirtualDom thread
            // (dioxus-core tasks.rs — "on the same thread as the rest of the
            // VirtualDom"), i.e. this thread, so a multi-megabyte file read
            // inside it would freeze the window just as before.
            let reporter = notify_desktop.clone();
            let read_path = path.clone();
            spawn(async move {
                let mut tabs = tabs;
                let mut active = active;

                let read = tokio::task::spawn_blocking(move || {
                    check_openable(&read_path)?;
                    read_file_to_string(&read_path)
                });
                let text = match read.await {
                    Ok(Ok(text)) => text,
                    Ok(Err(error)) => {
                        notify_error(reporter, notice, format!("打开文件失败：{error}"));
                        return;
                    }
                    Err(join) => {
                        notify_error(reporter, notice, format!("打开文件失败：{join}"));
                        return;
                    }
                };

                // The wait was not atomic: a second click on the same file may
                // have opened it while this read was in flight.
                let Ok(mut list) = tabs.try_write() else {
                    return;
                };
                if let Some(idx) = list.iter().position(|t| t.path == path) {
                    drop(list);
                    if let Ok(mut a) = active.try_write() {
                        *a = idx;
                    }
                    return;
                }

                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "未知".to_string());
                list.push(EditorTab {
                    path: path.clone(),
                    name,
                    dirty: false,
                    content: text,
                });
                let idx = list.len() - 1;
                // Semicolon, not a tail expression: as written, the `Result`'s
                // temporary outlives `active` and the block fails to borrow.
                if let Ok(mut a) = active.try_write() {
                    *a = idx;
                };
            });
        }
    };

    rsx! {
        style { {APP_CSS} }
        style { {TERMINAL_CSS} }
        style { {font_face_css()} }
        div { class: root_class,
            // Top bar. The only place that names the folder the app is pointed
            // at, and the only way to change it.
            div { class: "app-header",
                span { class: "app-brand", "Zmstop" }
                div { class: "app-workspace",
                    span { class: "app-workspace-label", "终端目录" }
                    span {
                        class: "app-workspace-name",
                        title: ws_title,
                        {ws_name}
                    }
                }
                button {
                    class: "header-btn",
                    title: "选择工作区文件夹",
                    onclick: move |_ev: Event<MouseData>| {
                        let start = terminal_cwd.peek().clone();
                        if let Some(dir) = choose_workspace(&dialog_reporter, &start, notice) {
                            directory_request.set(Some(dir));
                        }
                    },
                    "打开文件夹"
                }
            }
            div { class: "workspace",
                // File tree sidebar
                FileTree {
                    root: terminal_cwd,
                    on_open: open_file,
                    dark_mode: dark_mode,
                    width: file_tree_width,
                }
                div {
                    class: "workspace-resize-handle",
                    role: "separator",
                    "aria-orientation": "vertical",
                    "aria-label": "调整文件树宽度",
                    tabindex: "0",
                }
                // Main content area
                div { class: "main-area",
                    // Transient error banner
                    if !notice().is_empty() {
                        div { class: "toast",
                            onclick: move |_| notice.set(String::new()),
                            {notice().clone()}
                        }
                    }
                    // Editor panel
                    if editor_open {
                        EditorPanel {
                            dark_mode: dark_mode,
                            tabs: editor_tabs,
                            active_tab: active_editor_tab,
                            notice: notice,
                        }
                    }
                    // Terminal pane: a bottom dock while a tab is open, and
                    // always mounted — `hidden` only removes it from the paint,
                    // so a minimise costs the shells nothing and hands the whole
                    // height back to the editor, with no dead strip left over.
                    // With no tab open there is no editor to hand it back to, so
                    // the pane takes the column itself (`.solo`).
                    div {
                        class: if !terminal_visible() {
                            "terminal-panel hidden"
                        } else if editor_open {
                            "terminal-panel"
                        } else {
                            "terminal-panel solo"
                        },
                        style: panel_style,
                        // The split only exists when there is something above it
                        // to split from. Alone, the pane has no top edge worth
                        // drawing and nothing to drag.
                        if editor_open {
                            div { class: "terminal-resize-handle" }
                        }
                        Terminal {
                            dark_mode: dark_mode,
                            terminal_height: terminal_height,
                            cwd: terminal_cwd,
                            directory_request: directory_request,
                            visible: terminal_visible,
                            close_request: terminal_close,
                        }
                    }
                    // The way back from a minimise. Never advertised on a fresh
                    // launch, because the pane starts out open.
                    if !terminal_visible() {
                        div {
                            class: "terminal-restore",
                            title: "重新显示终端（会话仍在运行）",
                            onclick: move |_| terminal_visible.set(true),
                            span { class: "terminal-restore-icon", "▴" }
                            span { class: "terminal-restore-label", "终端已最小化 · 点击恢复" }
                        }
                    }
                    // About dialog
                    if about_open() {
                        div {
                            class: "about-backdrop",
                            onclick: move |_| about_open.set(false),
                            div {
                                class: "about-card",
                                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                                h2 { class: "about-title", "Zmstop" }
                                p { class: "about-version", {about_version} }
                                p { class: "about-text", "Dioxus 桌面客户端，内置代码编辑器与多标签 PTY 终端。" }
                                ul { class: "about-list",
                                    li { "Ctrl+Q 退出应用" }
                                    li { "Ctrl+S 保存当前文件" }
                                    li { "Ctrl+Shift+T 切换日间/夜间主题" }
                                    li { "Ctrl+` 显示 / 最小化终端（会话保留）" }
                                    li { "终端上的 ▾ 最小化，✕ 结束全部会话" }
                                    li { "拖拽终端上边缘的分隔条可调整上下比例" }
                                    li { "文件树目录跟随当前终端" }
                                    li { "Ctrl + / Ctrl - 调整编辑器与终端字号" }
                                    li { "Ctrl 0 重置编辑器字号（查看菜单也可调）" }
                                    li { "点击左侧文件树打开文件编辑" }
                                }
                                button {
                                    class: "about-close",
                                    onclick: move |_| about_open.set(false),
                                    "关闭"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::workspace_label;
    use std::path::Path;

    #[test]
    fn workspace_label_uses_the_final_segment() {
        assert_eq!(workspace_label(Path::new(r"D:\code\my-app")), "my-app");
        assert_eq!(workspace_label(Path::new("/home/me/proj")), "proj");
    }

    #[test]
    fn workspace_label_falls_back_for_a_filesystem_root() {
        // A root has no final component, so the header must show the whole path
        // rather than an empty box.
        assert_eq!(workspace_label(Path::new(r"C:\")), r"C:\");
        assert_eq!(workspace_label(Path::new("/")), "/");
    }
}