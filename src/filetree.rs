use dioxus::events::MouseData;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use crate::icons::icon_for;
use crate::util::strip_verbatim;

/// How often the tree re-reads the directories it is already showing.
///
/// The shell in the next pane creates files without telling us. A watcher
/// crate would fix it but would also be a dependency, a thread and a shutdown
/// protocol for one sidebar; one `read_dir` per visible directory on a timer
/// is cheaper and cannot leak.
const POLL_INTERVAL: Duration = Duration::from_millis(1200);

#[derive(Clone, PartialEq)]
struct TreeEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    ext: String,
    size: u64,
}

/// Directory listings keyed by path.
///
/// Reading the disk from render made every re-render — a theme toggle, a
/// terminal drag, one keystroke in the editor — walk the whole expanded tree
/// synchronously on the UI thread. A directory is now read when it is revealed
/// and re-read on a short timer (and by the refresh button), so rendering is
/// pure lookups: only applying a different listing re-renders a row.
type Listings = HashMap<PathBuf, Vec<TreeEntry>>;

/// Reads one directory, sorted. Errors are returned for display in the tree
/// rather than being presented as an empty directory.
fn read_entries(path: &Path) -> Result<Vec<TreeEntry>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("无法读取目录 {}：{error}", path.display()))?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("无法读取目录项 {}：{error}", path.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let (ext, size) = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => (
                path.extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_default(),
                metadata.len(),
            ),
            _ => (String::new(), 0),
        };
        result.push(TreeEntry {
            name,
            path,
            is_dir,
            ext,
            size,
        });
    }
    // Directories first, then case-insensitive by name. `sort_by_cached_key`
    // builds each key once instead of once per comparison.
    result.sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    Ok(result)
}

async fn read_listing(path: PathBuf) -> Result<Vec<TreeEntry>, String> {
    tokio::task::spawn_blocking(move || read_entries(&path))
        .await
        .map_err(|error| format!("目录读取任务失败：{error}"))?
}

async fn read_listings(paths: Vec<PathBuf>) -> Vec<(PathBuf, Result<Vec<TreeEntry>, String>)> {
    let error_paths = paths.clone();
    match tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let entries = read_entries(&path);
                (path, entries)
            })
            .collect()
    })
    .await
    {
        Ok(results) => results,
        Err(error) => error_paths
            .into_iter()
            .map(|path| (path, Err(format!("目录扫描任务失败：{error}"))))
            .collect(),
    }
}

/// Replaces the cached listing for `path` only if it actually differs.
///
/// Returns whether the cache changed. The poll that calls this walks every
/// known directory every second or so; writing an identical listing back would
/// mark the signal dirty and re-render the entire tree for nothing. That check
/// is what keeps the whole feature free for the common case of "nothing was
/// typed outside the app".
fn apply_listing(
    cache: &mut Listings,
    errors: &mut HashMap<PathBuf, String>,
    path: &Path,
    entries: Result<Vec<TreeEntry>, String>,
) -> bool {
    match entries {
        Ok(entries) => {
            let mut changed = errors.remove(path).is_some();
            match cache.get(path) {
                Some(have) if *have == entries => {}
                _ => {
                    cache.insert(path.to_path_buf(), entries);
                    changed = true;
                }
            }
            changed
        }
        Err(error) => {
            let mut changed = cache.remove(path).is_some();
            if errors.get(path) != Some(&error) {
                errors.insert(path.to_path_buf(), error);
                changed = true;
            }
            changed
        }
    }
}

fn listing<'a>(cache: &'a Listings, path: &Path) -> &'a [TreeEntry] {
    cache
        .get(path)
        .map(Vec::as_slice)
        .unwrap_or(&[] as &[TreeEntry])
}

/// `PathBuf` has no `Display`, and `{:?}` would leak quotes into the attribute.
fn key_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[component]
pub fn FileTree(
    root: Signal<PathBuf>,
    on_open: EventHandler<PathBuf>,
    dark_mode: Signal<bool>,
    width: Signal<u32>,
) -> Element {
    let mut listings = use_signal(Listings::default);
    let mut read_errors = use_signal(HashMap::<PathBuf, String>::default);
    let mut expanded = use_signal(HashSet::<PathBuf>::new);
    // Last root this tree was built for. `None` until the effect first runs, so
    // the very first run always loads the directory. Initialising it to the
    // root already in hand made the guard read as "loaded" on mount, and the
    // tree stayed empty until something else changed the root.
    // `peek`/`set` are used rather than `read`/`write` so this effect does not
    // subscribe to it — otherwise the `set` below would re-trigger it on every
    // render.
    let mut seen_root = use_signal(|| None::<PathBuf>);

    // Re-reads only when the root actually changes, not on every render.
    {
        use_effect(move || {
            let path = root();
            if seen_root.peek().as_deref() == Some(path.as_path()) {
                return;
            }
            seen_root.set(Some(path.clone()));
            // A new workspace (打开文件夹): the previous one's expanded rows and
            // cached listings name paths that no longer exist, so drop both.
            // Without this the tree would also keep stale entries alive for as
            // long as the app runs.
            expanded.write().clear();
            if let Ok(mut cache) = listings.try_write() {
                cache.clear();
            }
            if let Ok(mut errors) = read_errors.try_write() {
                errors.clear();
            }
            let root = root;
            let mut listings = listings;
            let mut read_errors = read_errors;
            spawn(async move {
                let entries = read_listing(path.clone()).await;
                if root.peek().as_path() != path.as_path() {
                    return;
                }
                if let Ok(mut cache) = listings.try_write() {
                    if let Ok(mut errors) = read_errors.try_write() {
                        apply_listing(&mut cache, &mut errors, &path, entries);
                    }
                }
            });
        });
    }

    // The shell in the next pane creates files without telling us, and the
    // root may be a directory the user is about to populate. A timer rather
    // than a watcher: one `read_dir` per directory already on screen, no extra
    // dependency, no thread to shut down, nothing that can leak.
    let mut fs_tick = use_signal(|| 0u64);
    {
        use_future(move || async move {
            let mut timer = interval(POLL_INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                timer.tick().await;
                *fs_tick.write() += 1;
            }
        });
    }

    // Disk enumeration and metadata are done on Tokio's blocking pool; the
    // VirtualDom thread only applies changed results to its cache.
    {
        use_effect(move || {
            let _ = fs_tick();
            let root_path = root();
            let mut known: Vec<PathBuf> = listings.peek().keys().cloned().collect();
            known.extend(read_errors.peek().keys().cloned());
            known.sort();
            known.dedup();
            if !known.contains(&root_path) {
                known.push(root_path.clone());
            }
            let mut listings = listings;
            let mut read_errors = read_errors;
            let root = root;
            spawn(async move {
                let entries = read_listings(known).await;
                if root.peek().as_path() != root_path.as_path() {
                    return;
                }
                if let Ok(mut cache) = listings.try_write() {
                    if let Ok(mut errors) = read_errors.try_write() {
                        for (path, entries) in entries {
                            apply_listing(&mut cache, &mut errors, &path, entries);
                        }
                    }
                }
            });
        });
    }

    // Expanding a folder is the moment we pay for reading it.
    let toggle = move |ev: PathBuf| {
        let mut is_open = expanded.write();
        if !is_open.remove(&ev) {
            is_open.insert(ev.clone());
            drop(is_open);
            let path = ev.clone();
            let mut listings = listings;
            let mut read_errors = read_errors;
            let expanded = expanded;
            let root = root;
            spawn(async move {
                let entries = read_listing(path.clone()).await;
                if !expanded.peek().contains(&ev) || !ev.starts_with(root.peek().as_path()) {
                    return;
                }
                if let Ok(mut cache) = listings.try_write() {
                    if let Ok(mut errors) = read_errors.try_write() {
                        apply_listing(&mut cache, &mut errors, &ev, entries);
                    }
                }
            });
        }
    };

    let root_path = root();
    let roots = listing(&listings.read(), &root_path).to_vec();
    let root_error = read_errors.read().get(&root_path).cloned();
    let dark = dark_mode();

    // `canonicalize` hands back a verbatim `\\?\D:\...` path, which the RTL
    // truncation below would mangle into a trailing `\?\\` — see
    // `util::strip_verbatim`. The U+200E mark then pins the run to left-to-right
    // so a UNC path's leading `\\` cannot be reordered to the end either,
    // while the ellipsis still comes from the left and keeps the leaf folder.
    let root_label = strip_verbatim(&root_path);
    let root_title = root_label.display().to_string();
    let root_text = format!("\u{200E}{}", root_label.display());
    let tree_style = format!("width: {}px", width());

    rsx! {
        div {
            // The stylesheet keys the day/night palette off `.file-tree.dark`,
            // so the modifier has to be on the root, not on the rows.
            class: if dark { "file-tree dark" } else { "file-tree" },
            style: tree_style,
            div {
                class: "file-tree-header",
                span {
                    class: "file-tree-path",
                    title: "{root_title}",
                    "{root_text}"
                }

                button {
                    class: "file-tree-refresh",
                    title: "刷新目录",
                    // Immediate pass over everything we already show, so the
                    // user does not have to wait out the poll interval.
                    onclick: move |_ev: Event<MouseData>| {
                        let root_path = root();
                        let mut known: Vec<PathBuf> =
                            listings.peek().keys().cloned().collect();
                        known.extend(read_errors.peek().keys().cloned());
                        known.sort();
                        known.dedup();
                        if !known.contains(&root_path) {
                            known.push(root_path.clone());
                        }
                        let mut listings = listings;
                        let mut read_errors = read_errors;
                        let root = root;
                        spawn(async move {
                            let entries = read_listings(known).await;
                            if root.peek().as_path() != root_path.as_path() {
                                return;
                            }
                            if let Ok(mut cache) = listings.try_write() {
                                if let Ok(mut errors) = read_errors.try_write() {
                                    for (path, entries) in entries {
                                        apply_listing(&mut cache, &mut errors, &path, entries);
                                    }
                                }
                            }
                        });
                    },
                    "⟳"
                }
            }
            div {
                class: "file-tree-body",
                for entry in roots.iter() {
                    TreeNode {
                        key: "{key_of(&entry.path)}",
                        entry: entry.clone(),
                        depth: 0,
                        listings,
                        read_errors,
                        expanded,
                        on_toggle: toggle,
                        on_open,
                        dark_mode: dark,
                    }
                }
                if roots.is_empty() {
                    if let Some(error) = root_error {
                        div { class: "file-tree-error", "{error}" }
                    } else {
                        div { class: "file-tree-empty", "（空目录）" }
                    }
                }
            }
        }
    }
}

/// One row of the tree, plus (when a directory is expanded) its children.
///
/// The expanded set arrives as a `Signal`, not as a cloned `HashSet` per node:
/// passing the set itself made rendering quadratic in the number of visible
/// rows.
#[component]
fn TreeNode(
    entry: TreeEntry,
    depth: usize,
    listings: Signal<Listings>,
    read_errors: Signal<HashMap<PathBuf, String>>,
    expanded: Signal<HashSet<PathBuf>>,
    on_toggle: EventHandler<PathBuf>,
    on_open: EventHandler<PathBuf>,
    dark_mode: bool,
) -> Element {
    let is_expanded = expanded.read().contains(&entry.path);

    // Short-lived guard: cloned out and released before the children render,
    // so a sibling can write to `listings` without tripping the borrow check.
    let children: Vec<TreeEntry> = if entry.is_dir && is_expanded {
        listing(&listings.read(), &entry.path).to_vec()
    } else {
        Vec::new()
    };
    let child_error = if is_expanded && children.is_empty() {
        read_errors.read().get(&entry.path).cloned()
    } else {
        None
    };

    let indent = 10 + depth * 14;
    // Complete `<svg>` markup for this entry. `icons.rs` is generated, so the
    // extension table and the artwork cannot drift apart the way a hand-written
    // match here would.
    let icon = icon_for(&entry.name, &entry.ext, entry.is_dir, is_expanded);

    // Only reaches paths the theme gave no colour of their own (they carry
    // `fill="currentColor"`), so a generic document or folder still follows the
    // day/night palette while branded icons keep their brand colour.
    let color = if entry.is_dir {
        "#d4b998"
    } else {
        match entry.ext.as_str() {
            "rs" => "#e28d6f",
            "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => "#e6c56f",
            "py" => "#7fb3d5",
            "md" => "#8fbc94",
            "json" | "toml" | "yaml" | "yml" => "#c9a26d",
            "html" | "css" => "#c08a7a",
            _ => {
                if dark_mode {
                    "#c6c8d1"
                } else {
                    "#5c6472"
                }
            }
        }
    };

    let title = if entry.is_dir {
        entry.name.clone()
    } else {
        format!("{} · {}", entry.name, human_size(entry.size))
    };

    rsx! {
        div {
            // A plain wrapper: the row is the only thing that should react to the
            // pointer, and it is the only thing that should carry the indent.
            class: "file-tree-node",
            div {
                class: "file-tree-row",
                style: "padding-left: {indent}px",
                title: "{title}",
                onclick: move |_ev| {
                    if entry.is_dir {
                        on_toggle.call(entry.path.clone());
                    } else {
                        on_open.call(entry.path.clone());
                    }
                },
                span {
                    class: "file-tree-icon",
                    style: "color: {color}",
                    // The theme's SVGs carry gradients, `<use>` references and
                    // per-icon viewBoxes; flattening them to `<path>`s would drop
                    // all of that, so the markup is injected as-is.
                    dangerous_inner_html: "{icon}"
                }
                span { class: "file-tree-label", "{entry.name}" }
            }
            for child in children.iter() {
                TreeNode {
                    key: "{key_of(&child.path)}",
                    entry: child.clone(),
                    depth: depth + 1,
                    listings,
                    read_errors,
                    expanded,
                    on_toggle,
                    on_open,
                    dark_mode,
                }
            }
            if let Some(error) = child_error {
                div {
                    class: "file-tree-error",
                    style: "padding-left: {indent}px",
                    "{error}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the system temp folder, tagged with the
    /// process id so two test binaries running at once cannot collide.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maid-filetree-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn refresh_sees_everything_added_since_the_last_read() {
        let dir = scratch("created");
        let mut cache = Listings::default();
        let mut errors = HashMap::new();

        assert!(apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
        assert!(listing(&cache, &dir).is_empty());

        std::fs::create_dir(dir.join("alpha")).expect("mkdir");
        std::fs::write(dir.join("note.txt"), b"hi").expect("write");

        assert!(apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
        let entries = listing(&cache, &dir);
        // Directories sort ahead of files, which is the order the rows show.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alpha");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "note.txt");
        assert_eq!(entries[1].ext, "txt");

        // Unchanged on a second pass: this is what stops the poll from
        // re-rendering the whole tree every tick for no reason.
        assert!(!apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refresh_forgets_a_directory_that_has_gone() {
        let dir = scratch("deleted");
        std::fs::create_dir(dir.join("child")).expect("mkdir");

        let mut cache = Listings::default();
        let mut errors = HashMap::new();
        assert!(apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
        assert!(cache.contains_key(&dir));

        std::fs::remove_dir_all(&dir).expect("rmdir");

        // Deleted on disk must mean deleted from the cache, or the row would
        // outlive the directory until the app restarted.
        assert!(apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
        assert!(!cache.contains_key(&dir));
        // ...and there is nothing left for a further poll to do.
        assert!(!apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
    }

    #[test]
    fn unreadable_directory_is_reported_instead_of_looking_empty() {
        let dir = scratch("unreadable");
        std::fs::remove_dir_all(&dir).expect("remove scratch directory");
        let mut cache = Listings::default();
        let mut errors = HashMap::new();

        assert!(apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
        assert!(listing(&cache, &dir).is_empty());
        assert!(errors
            .get(&dir)
            .is_some_and(|error| error.contains("无法读取目录")));
        assert!(!apply_listing(
            &mut cache,
            &mut errors,
            &dir,
            read_entries(&dir)
        ));
    }

    /// The generator already fails loudly on a missing lookup; this is the
    /// runtime half of that guarantee, so a stale or hand-edited `icons.rs`
    /// surfaces as an empty icon column rather than silently.
    #[test]
    fn known_entries_resolve_to_a_real_icon() {
        // A file name beats its own extension rule: README gets its own mark,
        // not the generic one every other `.md` gets.
        assert_ne!(
            icon_for("readme.md", "md", false, false),
            icon_for("notes.md", "md", false, false)
        );

        // Extensions reach their curated icon rather than the generic file.
        let generic = icon_for("mystery.zebra", "zebra", false, false);
        for ext in ["rs", "py", "json", "toml", "sh"] {
            assert_ne!(icon_for("entry", ext, false, false), generic, "{ext}");
        }

        // Directories get their own artwork, and the open variant differs from
        // the closed one — the pair the expanded row uses.
        let folder = icon_for("src", "", true, false);
        assert_ne!(folder, generic);
        assert_ne!(icon_for("src", "", true, true), folder);

        // Anything unrecognised still gets *something* to render.
        assert!(generic.contains("<svg"));
        assert!(generic.contains("</svg>"));
    }
}
