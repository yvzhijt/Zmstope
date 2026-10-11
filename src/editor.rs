//! Code editor panel powered by CodeMirror 6.
//!
//! One editor instance serves every open file. Who owns what:
//!
//! * Rust owns each tab's **saved** text (`EditorTab::content`), i.e. what is on
//!   disk when the tab was opened.
//! * The webview owns each tab's **live** buffer, keyed by file path.
//!
//! That split is what makes tab switching behave: the editor is pointed at the
//! right path and its live buffer wins over the disk text, so unsaved edits
//! survive a switch instead of the panel showing whatever file was opened last.
//! The panel also has to destroy the view on unmount — it is created lazily and
//! the container element goes away with the component, so a stale instance would
//! leave the next mount permanently blank.

use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use dioxus_desktop::{use_muda_event_handler, use_window, DesktopContext};

use crate::util::{base64_encode, escape_js_string};

// ── JS bridge ───────────────────────────────────────────────────────

const CODEMIRROR_JS: &str = include_str!("../assets/codemirror.js");

static CODEMIRROR_BOOTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// The bundle is ~1.5 MB, so it is injected at most once per process. The view
/// itself is rebuilt on every mount; only the global `CodeMirrorBundle` sticks.
fn boot_codemirror(desktop: &DesktopContext) {
    CODEMIRROR_BOOTED.get_or_init(|| {
        let _ = desktop
            .webview
            .evaluate_script(&format!("(function(){{{CODEMIRROR_JS};}})();"));
    });
}

/// JS → Rust channel. Carries exactly two message kinds: `dirty`, to keep
/// `EditorTab::truthful`, and `save`, which is how Ctrl+S reaches Rust.
///
/// The menu accelerator cannot deliver it. muda builds a real `HACCEL` table
/// (`Menu::haccel`), but Windows only translates one when the message loop
/// calls `TranslateAcceleratorW`, and neither tao 0.26 nor dioxus-desktop 0.5
/// ever does — so on Windows *every* `acc(...)` in `create_menu` is inert and
/// the item fires only from a mouse click. Handling the key inside the webview
/// is the only route that exists, and it is scoped so a terminal with focus
/// keeps its own Ctrl+S (bash flow control) instead of saving over it.
const EDITOR_BRIDGE_JS: &str = r#"
window.__editor = window.__editor || {};
window.__editor.ch = dioxus;
window.__editor.post = function (m) {
  try { window.__editor.ch.send(m); } catch (e) {}
};

if (!window.__editor.saveHotkey) {
  window.__editor.saveHotkey = true;
  window.addEventListener('keydown', function (ev) {
    if (!(ev.ctrlKey || ev.metaKey) || ev.shiftKey || ev.altKey) return;
    if (ev.key.toLowerCase() !== 's') return;
    var e = window.__editor;
    if (!e) return;
    if (ev.target && typeof ev.target.closest === 'function' &&
        ev.target.closest('.terminal-root')) return;
    // Claimed even with nothing to save, so the key cannot fall through to
    // whatever the host webview would do with it instead.
    ev.preventDefault();
    ev.stopPropagation();
    if (ev.repeat) return;
    if (e.instance && e.instance.activeKey && e.post) e.post({ t: 'save' });
  }, true);
}
"#;

/// Creates the view once the container and the bundle are both present.
///
/// A load request that arrives before the view exists is parked in
/// `__editor.pending` and replayed here, which closes the window between Rust
/// deciding to render the editor and CodeMirror being ready to take it.
const EDITOR_CREATE_JS: &str = r#"
(function() {
  window.__editor = window.__editor || {};
  var attempts = 0;

  window.__editor.applyPending = function () {
    var e = window.__editor;
    if (!e.instance || !e.pending) return;
    var p = e.pending;
    e.pending = null;
    e.instance.load(p.key, p.text, p.ext, p.dark);
  };

  function initEditor() {
    var container = document.getElementById('codemirror-root');
    if (!container || typeof CodeMirrorBundle === 'undefined') {
      // The container comes from this render pass and the bundle from the
      // sibling effect, so both may lag by a tick. Bounded, so a real load
      // failure stops instead of polling for the life of the app.
      if (attempts++ < 100) { setTimeout(initEditor, 100); return; }
      window.__editor.failed = true;
      console.error('[Zmstop] CodeMirror 未加载，编辑器不可用');
      return;
    }
    if (window.__editor.instance) return;
    try {
      window.__editor.instance = CodeMirrorBundle.create(container, {
        text: '', ext: '', dark: false,
        onDirty: function (dirty) {
          if (window.__editor.post) window.__editor.post({ t: 'dirty', dirty: !!dirty });
        }
      });
      window.__editor.applyPending();
    } catch (e) {
      console.error('[Zmstop] CodeMirror 初始化失败:', e);
      if (attempts++ < 100) setTimeout(initEditor, 200);
    }
  }

  initEditor();
})();
"#;

/// Runs when the panel unmounts. Leaving `window.__editor.instance` set is what
/// used to break the editor for good: the next mount's `initEditor` saw a live
/// instance and skipped creation, so its brand new `#codemirror-root` stayed
/// empty while the real view was still parented to a detached node.
const EDITOR_DISPOSE_JS: &str = r#"
(function() {
  var e = window.__editor;
  if (!e) return;
  e.pending = null;
  e.failed = false;
  if (e.instance) {
    try { e.instance.destroy(); } catch (x) {}
    e.instance = null;
  }
  e.ch = null;
})();
"#;

/// Points the editor at one file. `key` is the path; JS uses it as the buffer key.
fn editor_load_js(key: &str, text: &str, ext: &str, dark: bool) -> String {
    let text_b64 = base64_encode(text.as_bytes());
    format!(
        r#"(function(){{
  var e = window.__editor;
  if (!e) return;
  // atob() returns a Latin-1 string; we need UTF-8 for Chinese characters.
  var raw = atob('{text_b64}');
  var bytes = new Uint8Array(raw.length);
  for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  var text = new TextDecoder().decode(bytes);
  e.pending = {{ key: '{key}', text: text, ext: '{ext}', dark: {dark} }};
  if (e.applyPending) e.applyPending();
}})();"#,
        key = escape_js_string(key),
        ext = escape_js_string(ext),
    )
}

/// Drops a tab's live buffer, so reopening the file starts from disk again.
fn editor_forget_js(key: &str) -> String {
    format!(
        "(function(){{var e=window.__editor;if(e&&e.instance)delete e.instance.buffers['{}'];}})()",
        escape_js_string(key)
    )
}

/// Runs as a Dioxus `eval`, so its resolved value is the live state of the
/// whole panel (or `null` when there is no view yet).
///
/// Returns all three pieces because the view only ever shows one tab: `active`
/// and `text` describe the document on screen, which is authoritative for that
/// file, while `buffers` carries every other tab's text — `editor.js` writes
/// each tab's buffer on every document change, so a save targeting a tab that
/// is not on screen still gets its real content instead of the disk copy.
fn editor_text_js() -> &'static str {
    r#"
var e = window.__editor;
if (!e || !e.instance) return null;
var i = e.instance;
return {
  active: i.activeKey,
  text: i.view ? i.view.state.doc.toString() : null,
  buffers: i.buffers
};
"#
}

/// Changes the editor font size from outside the view (the 查看 menu).
///
/// The zoom level lives in the bundle rather than in the instance, so this
/// reaches every open view and survives the next one being created; that is why
/// it goes through `changeFontSize` instead of poking `view.dom.style` here.
/// `delta == 0` resets to the size the stylesheet asks for.
pub fn editor_zoom_js(delta: i32) -> String {
    format!(
        r#"(function(){{
  var e = window.__editor;
  if (e && e.instance) e.instance.changeFontSize({delta});
}})();"#
    )
}

// ── State ───────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub struct EditorTab {
    pub path: PathBuf,
    pub name: String,
    pub dirty: bool,
    /// Text as it was on disk when the tab was opened. Never updated by typing,
    /// so it doubles as the baseline for "is this tab dirty".
    pub content: String,
}

// ── Component ───────────────────────────────────────────────────────

#[component]
pub fn EditorPanel(
    dark_mode: Signal<bool>,
    mut tabs: Signal<Vec<EditorTab>>,
    active_tab: Signal<usize>,
    notice: Signal<String>,
) -> Element {
    let desktop = use_window();
    let bridge = use_hook(|| eval(EDITOR_BRIDGE_JS));

    // Paths whose live buffer has to be dropped. A queue instead of a direct
    // call because rsx event handlers are `FnMut`, so they cannot own a
    // `DesktopContext` (it is not `Copy`).
    let mut forget = use_signal(Vec::<PathBuf>::new);
    // A dirty tab the user asked to close. Held instead of closing right away
    // so the prompt can offer 保存 / 放弃 / 取消; `None` means no prompt.
    let mut pending_close = use_signal(|| None::<PathBuf>);
    {
        let desktop = desktop.clone();
        use_effect(move || {
            let paths = forget();
            if paths.is_empty() {
                return;
            }
            for path in &paths {
                let _ = desktop
                    .webview
                    .evaluate_script(&editor_forget_js(&path.to_string_lossy()));
            }
            if let Ok(mut queue) = forget.try_write() {
                queue.clear();
            }
        });
    }

    // Boot the bundle and build the view.
    {
        let desktop = desktop.clone();
        use_effect(move || {
            boot_codemirror(&desktop);
            let _ = desktop.webview.evaluate_script(EDITOR_CREATE_JS);
        });
    }

    // Destroy the view when the panel goes away (last tab closed, app closing).
    {
        let desktop = desktop.clone();
        use_drop(move || {
            let _ = desktop.webview.evaluate_script(EDITOR_DISPOSE_JS);
        });
    }

    // Point the editor at the active file. Reads `tabs` and `dark_mode`, so this
    // also runs when the theme flips — `setTheme` alone does not disturb the doc.
    {
        let desktop = desktop.clone();
        use_effect(move || {
            let dark = dark_mode();
            let active = *active_tab.peek();
            let (key, text, ext) = {
                let Ok(list) = tabs.try_read() else { return };
                let Some(tab) = list.get(active) else { return };
                let key = tab.path.to_string_lossy().into_owned();
                let ext = tab
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_owned();
                (key, tab.content.clone(), ext)
            };
            let _ = desktop
                .webview
                .evaluate_script(&editor_load_js(&key, &text, &ext, dark));
        });
    }

    // Bridge messages. Started from an effect because `spawn` is not idempotent
    // across renders. Ends when the eval channel dies with the scope.
    {
        let active_tab = active_tab;
        let desktop = desktop.clone();
        use_effect(move || {
            let mut bridge = bridge;
            let desktop = desktop.clone();
            spawn(async move {
                while let Ok(message) = bridge.recv().await {
                    match message.get("t").and_then(|v| v.as_str()) {
                        Some("dirty") => {
                            let dirty = message
                                .get("dirty")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            // `try_write`, and only on an actual change: a write marks
                            // the component dirty, which would re-run the load effect
                            // above on every single keystroke otherwise.
                            let Ok(mut list) = tabs.try_write() else {
                                break;
                            };
                            let active = *active_tab.peek();
                            let Some(tab) = list.get_mut(active) else {
                                continue;
                            };
                            if tab.dirty != dirty {
                                tab.dirty = dirty;
                            }
                        }
                        // Ctrl+S from the webview. This is the route that works
                        // on Windows; see EDITOR_BRIDGE_JS.
                        Some("save") => save_active_tab(tabs, active_tab, &desktop, notice),
                        _ => {}
                    }
                }
            });
        });
    }

    // 文件 → 保存. Handling it here keeps the path lookup local; `App` ignores
    // the menu id. Kept alongside the webview route rather than replacing it:
    // macOS and Linux translate menu accelerators natively, so this one is the
    // working path there, while Windows only ever reaches us from a click.
    {
        let desktop = desktop.clone();
        let active_tab = active_tab;
        use_muda_event_handler(move |event| {
            if event.id.0.as_str() == "save" {
                save_active_tab(tabs, active_tab, &desktop, notice);
            }
        });
    }

    let current_tabs = tabs.read().clone();
    let active = *active_tab.peek();
    // Rows are copied out before `rsx!`. An event handler has to be `'static`,
    // so it cannot borrow the list it was rendered from — which is also why the
    // old ✕ handler re-read the tab by index instead of closing over the loop
    // variable.
    let tab_rows: Vec<(PathBuf, bool, String)> = current_tabs
        .iter()
        .map(|t| (t.path.clone(), t.dirty, t.name.clone()))
        .collect();

    // What the prompt says, resolved up front so the element tree only has to
    // test whether there is a string at all.
    let confirm_text = pending_close()
        .as_ref()
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            format!("{name} 中有尚未保存的更改。关闭这个标签页会丢失它们。")
        })
        .unwrap_or_default();
    // The save button needs a `DesktopContext` it can hand to `notify`, and an
    // `FnMut` handler cannot move a non-`Copy` value into its task each click.
    let prompt_reporter = desktop.clone();

    rsx! {
        div { class: "editor-panel",
            // Tab bar
            div { class: "editor-tab-bar",
                for (i, (path, dirty, name)) in tab_rows.into_iter().enumerate() {
                    div {
                        class: if i == active { "editor-tab active" } else { "editor-tab" },
                        onclick: move |_| {
                            if let Ok(mut a) = active_tab.try_write() {
                                *a = i;
                            }
                        },
                        span { class: "editor-tab-name",
                            "{name}"
                            if dirty {
                                " ●"
                            }
                        }
                        button {
                            class: "editor-tab-close",
                            onclick: move |e: Event<MouseData>| {
                                e.stop_propagation();
                                // A dirty tab never closes outright: the
                                // prompt decides, so a stray click on ✕ cannot
                                // throw the edits away.
                                if dirty {
                                    pending_close.set(Some(path.clone()));
                                } else {
                                    close_tab(tabs, active_tab, forget, &path);
                                }
                            },
                            "×"
                        }
                    }
                }
            }
            // Editor area
            div { class: "editor-area",
                id: "codemirror-root",
            }
            // Unsaved-changes prompt. `.editor-panel` is `position: relative`,
            // so the backdrop also covers the tab bar it hangs off.
            if !confirm_text.is_empty() {
                div { class: "confirm-backdrop",
                    onclick: move |_| pending_close.set(None),
                    div { class: "confirm-card",
                        onclick: move |e: Event<MouseData>| e.stop_propagation(),
                        h3 { class: "confirm-title", "有未保存的修改" }
                        p { class: "confirm-text", "{confirm_text}" }
                        div { class: "confirm-actions",
                            button {
                                class: "confirm-btn primary",
                                onclick: move |_| {
                                    let Some(path) = pending_close.peek().clone() else {
                                        return;
                                    };
                                    let name = path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                                    let reporter = prompt_reporter.clone();
                                    spawn(async move {
                                        match write_tab(tabs, &path).await {
                                            Ok(()) => {
                                                // Only dismiss the prompt once the
                                                // bytes are on disk: a failed save
                                                // must leave the dialog up so the
                                                // user can retry instead of having
                                                // to hunt for ✕ again.
                                                pending_close.set(None);
                                                crate::notify(
                                                    reporter,
                                                    notice,
                                                    format!("已保存 {name}"),
                                                );
                                                close_tab(tabs, active_tab, forget, &path);
                                            }
                                            // Nothing reached the disk, so the
                                            // tab stays: closing would discard
                                            // exactly what the user just chose
                                            // to keep.
                                            Err(error) => crate::notify_error(
                                                reporter,
                                                notice,
                                                format!("{name} 未保存：{error}"),
                                            ),
                                        }
                                    });
                                },
                                "保存并关闭"
                            }
                            button {
                                class: "confirm-btn",
                                onclick: move |_| {
                                    let Some(path) = pending_close.peek().clone() else {
                                        return;
                                    };
                                    pending_close.set(None);
                                    close_tab(tabs, active_tab, forget, &path);
                                },
                                "放弃修改"
                            }
                            button {
                                class: "confirm-btn",
                                onclick: move |_| pending_close.set(None),
                                "取消"
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Writes one tab's live text to disk and marks it clean.
///
/// Any tab can be targeted, not only the one on screen: `editor_text_js` hands
/// back the view's document for whichever file it is showing and the stored
/// buffer for every other tab, so the confirm dialog can save the very tab the
/// user is about to close. It shows nothing on success or failure — the
/// callers report, because "save" and "save and close" word it differently.
async fn write_tab(mut tabs: Signal<Vec<EditorTab>>, path: &Path) -> Result<(), String> {
    let Ok(value) = eval(editor_text_js()).join().await else {
        return Err("读取编辑器内容失败".to_owned());
    };
    let Some(state) = value.as_object() else {
        return Err("编辑器尚未就绪，无法保存".to_owned());
    };
    let key = path.to_string_lossy().into_owned();
    // The document on screen is authoritative for its own file; every other
    // tab comes from its buffer. The buffer is the fallback rather than the
    // first choice so a stale one can never overwrite what is on screen.
    let text = if state.get("active").and_then(|v| v.as_str()) == Some(key.as_str()) {
        state.get("text").and_then(|v| v.as_str())
    } else {
        None
    }
    .or_else(|| {
        state
            .get("buffers")
            .and_then(|b| b.get(key.as_str()))
            .and_then(|v| v.as_str())
    });
    let Some(text) = text else {
        return Err("编辑器尚未就绪，无法保存".to_owned());
    };

    let path = path.to_path_buf();
    let text = text.to_owned();
    let write_path = path.clone();
    let write_text = text.clone();
    tokio::task::spawn_blocking(move || write_file_from_string(&write_path, &write_text))
        .await
        .map_err(|error| format!("保存任务失败: {error}"))??;
    if let Ok(mut list) = tabs.try_write() {
        if let Some(entry) = list.iter_mut().find(|t| t.path == path) {
            entry.content = text;
            entry.dirty = false;
        }
    }
    Ok(())
}

/// Save for the tab on screen — the 文件 menu item and the webview's Ctrl+S.
///
/// Neither route reaches everywhere on its own: Windows never translates the
/// menu accelerator (nothing calls `TranslateAcceleratorW`), while the webview
/// key only exists while an editor tab is loaded.
fn save_active_tab(
    tabs: Signal<Vec<EditorTab>>,
    active_tab: Signal<usize>,
    desktop: &DesktopContext,
    notice: Signal<String>,
) {
    let Some(tab) = tabs.read().get(*active_tab.peek()).cloned() else {
        return;
    };
    let desktop = desktop.clone();
    spawn(async move {
        match write_tab(tabs, &tab.path).await {
            Ok(()) => crate::notify(desktop, notice, format!("已保存 {}", tab.name)),
            Err(error) => crate::notify_error(desktop, notice, error),
        }
    });
}

/// Drops `path` from the tab list and picks the tab that should show instead.
///
/// Shared by the plain ✕, 放弃修改 and 保存并关闭 so the index arithmetic above
/// the list — and the "queue the buffer for disposal" step — is written once.
fn close_tab(
    mut tabs: Signal<Vec<EditorTab>>,
    mut active_tab: Signal<usize>,
    mut forget: Signal<Vec<PathBuf>>,
    path: &Path,
) {
    let Ok(mut list) = tabs.try_write() else {
        return;
    };
    let Some(i) = list.iter().position(|t| t.path == path) else {
        return;
    };
    list.remove(i);
    if !list.is_empty() {
        let active = *active_tab.peek();
        let next = if active >= i && active > 0 {
            active - 1
        } else {
            active
        };
        if let Ok(mut a) = active_tab.try_write() {
            *a = next.min(list.len() - 1);
        }
    }
    drop(list);
    if let Ok(mut queue) = forget.try_write() {
        queue.push(path.to_path_buf());
    }
}

/// Hard cap on what the editor will load into memory.
///
/// Source files almost never reach 1 MB, while logs, databases and build
/// output run to hundreds. Reading one of those in meant handing the whole
/// window a copy of it. 20 MB sits where "clearly not a document" starts and
/// "still worth holding in one String" ends.
pub const MAX_OPEN_BYTES: u64 = 20 * 1024 * 1024;

fn too_big(len: u64) -> Option<String> {
    (len > MAX_OPEN_BYTES).then(|| {
        format!(
            "文件过大（{} MB），超过上限 {} MB。可以在终端里查看它。",
            len / (1024 * 1024),
            MAX_OPEN_BYTES / (1024 * 1024)
        )
    })
}

/// Quick pre-flight before reading: metadata only, no file contents.
///
/// The caller runs this on Tokio's blocking pool, so path metadata never stalls
/// the VirtualDom thread. The read re-checks the cap because preflight and read
/// cannot be one atomic filesystem operation.
pub fn check_openable(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("无法读取文件: {e}"))?;
    if meta.is_dir() {
        return Err("这是一个目录".to_owned());
    }
    if let Some(error) = too_big(meta.len()) {
        return Err(error);
    }
    Ok(())
}

/// Reads a file as UTF-8 text, refusing binary content.
///
/// Deliberately `open`/`take`/`from_utf8` rather than `read_to_string`, for
/// two reasons. First, the cap has to hold even if the file grew between the
/// pre-flight and now — `take` makes that impossible instead of merely
/// unlikely. Second, `read_to_string` reports every non-text file as
/// "stream did not contain valid UTF-8", which hides the case that actually
/// matters: a file with NUL bytes in it is not a text file at all, and
/// opening it and saving it back would corrupt the original.
pub fn read_file_to_string(path: &Path) -> Result<String, String> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|e| format!("无法读取文件: {e}"))?;
    if let Some(error) = too_big(
        file.metadata()
            .map_err(|e| format!("无法读取文件: {e}"))?
            .len(),
    ) {
        return Err(error);
    }

    let mut bytes = Vec::new();
    file.take(MAX_OPEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法读取文件: {e}"))?;
    // Only reachable when the file grew while it was being read.
    if let Some(error) = too_big(bytes.len() as u64) {
        return Err(error);
    }
    if bytes.contains(&0) {
        return Err("这是二进制文件，不是文本文档".to_owned());
    }
    String::from_utf8(bytes).map_err(|e| format!("不是有效的 UTF-8 文本: {e}"))
}

pub fn write_file_from_string(path: &Path, content: &str) -> Result<(), String> {
    std::fs::write(path, content).map_err(|e| format!("无法保存文件: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a file under the temp folder with a name that will not collide.
    fn scratch(tag: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("desktop-editor-{tag}"));
        std::fs::write(&path, bytes).expect("scratch file");
        path
    }

    #[test]
    fn opens_plain_utf8_text() {
        let path = scratch("plain", "第一行\n第二行\n".as_bytes());
        assert_eq!(read_file_to_string(&path).unwrap(), "第一行\n第二行\n");
        assert!(check_openable(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_a_directory() {
        let dir = std::env::temp_dir();
        let error = check_openable(&dir).expect_err("a directory is not a file");
        assert!(error.contains("目录"), "{error}");
        let _ = read_file_to_string(&dir).expect_err("reading a directory must fail");
    }

    #[test]
    fn refuses_an_oversized_file() {
        // Sparse: a 21 MB file without writing 21 MB. `set_len` is enough
        // because both checks read `metadata().len()`, not the file contents.
        let path = std::env::temp_dir().join("desktop-editor-huge");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(MAX_OPEN_BYTES + 1).expect("set_len");

        let error = check_openable(&path).expect_err("oversized files are refused");
        assert!(error.contains("文件过大"), "{error}");
        let error = read_file_to_string(&path).expect_err("the read re-checks too");
        assert!(error.contains("文件过大"), "{error}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_binary_content() {
        // NUL is what separates "not UTF-8" from "not text at all".
        let path = scratch("binary", &[0x89, b'P', 0x00, b'N', 0x47]);
        let error = read_file_to_string(&path).expect_err("binary files are refused");
        assert!(error.contains("二进制"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reports_invalid_utf8_as_a_encoding_problem() {
        // Valid-ish bytes with no NUL, so it reaches the UTF-8 check.
        let path = scratch("latin", &[0xC3, 0x28, b'a']);
        let error = read_file_to_string(&path).expect_err("invalid UTF-8 is refused");
        assert!(error.contains("UTF-8"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reports_a_missing_path() {
        let path = Path::new(r"C:\no\such\file-9f3a.desktop");
        let error = check_openable(path).expect_err("missing files are refused");
        assert!(error.contains("无法读取文件"), "{error}");
    }
}