import { EditorView, keymap, lineNumbers, highlightActiveLineGutter, highlightSpecialChars,
         drawSelection, dropCursor, rectangularSelection, crosshairCursor,
         highlightActiveLine } from '@codemirror/view';
import { EditorState, Compartment } from '@codemirror/state';
import { indentOnInput, bracketMatching, syntaxHighlighting,
         defaultHighlightStyle, foldGutter, indentUnit } from '@codemirror/language';
import { history, defaultKeymap, historyKeymap } from '@codemirror/commands';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { oneDark } from '@codemirror/theme-one-dark';

import { javascript } from '@codemirror/lang-javascript';
import { rust } from '@codemirror/lang-rust';
import { python } from '@codemirror/lang-python';
import { css } from '@codemirror/lang-css';
import { html } from '@codemirror/lang-html';
import { json } from '@codemirror/lang-json';
import { markdown } from '@codemirror/lang-markdown';
import { cpp } from '@codemirror/lang-cpp';

const langMap = {
  js: javascript, jsx: javascript, ts: javascript, tsx: javascript,
  rs: rust,
  c: cpp, h: cpp, cc: cpp, cpp: cpp, cxx: cpp, hpp: cpp, hh: cpp, hxx: cpp,
  py: python,
  css: css,
  html: html, htm: html,
  json: json,
  md: markdown, markdown: markdown
};

function langExtension(ext) {
  const factory = langMap[String(ext || '').toLowerCase()];
  return factory ? factory() : [];
}

// Matches the day palette of the app shell (`#172347` text on paper) so the
// editor does not stay dark while the rest of the window goes light.
const EDITOR_FONT = "'JetBrains Mono','Cascadia Code','Fira Code','Consolas','Courier New',monospace";

const lightTheme = EditorView.theme({
  '&': { backgroundColor: '#fbfcfe', color: '#172347', fontFamily: EDITOR_FONT },
  '.cm-content': { caretColor: '#172347', fontFamily: EDITOR_FONT },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: '#172347' },
  '.cm-gutters': { backgroundColor: '#f2f5fa', color: '#8a94a8', border: 'none', fontFamily: EDITOR_FONT },
  '.cm-activeLine': { backgroundColor: '#eef2fa' },
  '.cm-activeLineGutter': { backgroundColor: '#e4eaf6', color: '#172347' },
  '.cm-selectionBackground, .cm-content ::selection': { backgroundColor: '#c7d7f5' }
}, { dark: false });

const darkFontOverride = EditorView.theme({
  '&': { fontFamily: EDITOR_FONT },
  '.cm-content': { fontFamily: EDITOR_FONT },
  '.cm-gutters': { fontFamily: EDITOR_FONT }
}, { dark: true });

function themeExtension(dark) {
  return dark ? [oneDark, darkFontOverride] : lightTheme;
}

// Language and theme are the two things that change while the app runs, so they
// live in compartments and are swapped with `reconfigure` instead of forcing a
// brand new EditorView (which would throw away the cursor and the undo history).
const langCompartment = new Compartment();
const themeCompartment = new Compartment();

const coreExtensions = [
  lineNumbers(), highlightActiveLineGutter(), highlightSpecialChars(),
  history(), foldGutter(), drawSelection(), dropCursor(),
  EditorState.allowMultipleSelections.of(true), indentOnInput(),
  syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
  bracketMatching(), closeBrackets(), rectangularSelection(), crosshairCursor(),
  highlightActiveLine(), highlightSelectionMatches(),
  indentUnit.of('  ')
];

// The base the UI scale is applied to, so the effective default is
// round(BASE_FONT_PX * --font-scale) = round(16 * 1.25) = 20px. Only this
// number moves the editor's size; the bounds below stay 9–40px either way.
const BASE_FONT_PX = 16;
const MIN_FONT_PX = 9;
const MAX_FONT_PX = 40;
const ZOOM_STEP_PX = 1;
const ZOOM_MIN_STEPS = -(BASE_FONT_PX - MIN_FONT_PX);
const ZOOM_MAX_STEPS = MAX_FONT_PX - BASE_FONT_PX;
const ZOOM_STORE_KEY = 'maid.editor.zoom';

/**
 * The app stylesheet keeps the UI type scale in `--font-scale`; reading it here
 * means one number drives the shell, the editor and the terminal together.
 */
function baseFontSize() {
  try {
    const raw = getComputedStyle(document.documentElement)
      .getPropertyValue('--font-scale');
    const scale = parseFloat(raw);
    if (scale > 0) return Math.round(BASE_FONT_PX * scale);
  } catch (e) { /* pre-CSS, or no documentElement: fall through */ }
  return BASE_FONT_PX;
}

// The zoom is app-wide and persistent on purpose. Each tab owns a separate
// EditorView with its own DOM node, so a per-instance font size would make the
// size you picked in one tab vanish in the next one, and would reset every time
// the last tab closed (which disposes the view).
let zoomSteps = 0;
const liveInstances = new Set();

function clampSteps(steps) {
  return Math.max(ZOOM_MIN_STEPS, Math.min(ZOOM_MAX_STEPS, steps));
}

(function restoreZoom() {
  try {
    const raw = parseInt(localStorage.getItem(ZOOM_STORE_KEY), 10);
    if (isFinite(raw)) zoomSteps = clampSteps(raw);
  } catch (e) { /* storage disabled: stay at the stylesheet size */ }
})();

function rememberZoom() {
  try { localStorage.setItem(ZOOM_STORE_KEY, String(zoomSteps)); }
  catch (e) { /* storage disabled: the size just will not survive a restart */ }
}

function zoomedFontSize() {
  const px = baseFontSize() + zoomSteps * ZOOM_STEP_PX;
  return Math.max(MIN_FONT_PX, Math.min(MAX_FONT_PX, px));
}

function applyZoom() {
  const px = zoomedFontSize();
  for (const inst of liveInstances) {
    if (inst.fontSize === px) continue;
    inst.fontSize = px;
    inst.view.dom.style.fontSize = px + 'px';
    // Cell metrics are cached, so without this the grid keeps the old row
    // height and the larger glyphs overflow their line boxes.
    inst.view.requestMeasure();
  }
}

function stepZoom(delta) {
  const before = zoomedFontSize();
  zoomSteps = clampSteps(zoomSteps + delta);
  if (zoomedFontSize() === before) return; // already pinned to a clamp
  rememberZoom();
  applyZoom();
}

function resetZoom() {
  if (zoomSteps === 0) return false;
  zoomSteps = 0;
  rememberZoom();
  applyZoom();
  return true;
}

/**
 * Key names come from `w3c-keyname`, which reports a printable key as the
 * character it types, so Ctrl+= arrives as "Ctrl-=" and Ctrl+Shift+= as
 * "Ctrl-+" -- never as "Ctrl-Equal". Every physical spelling of the zoom keys
 * therefore needs its own binding, or Ctrl++ / Ctrl+- silently do nothing.
 * `Mod-` resolves to Ctrl on Windows and Cmd on macOS.
 */
function zoomKeymap() {
  const zoomIn = () => { stepZoom(1); return true; };
  const zoomOut = () => { stepZoom(-1); return true; };
  return keymap.of([
    { key: 'Mod-=', run: zoomIn },
    { key: 'Mod-+', run: zoomIn },
    { key: 'Mod--', run: zoomOut },
    { key: 'Mod-_', run: zoomOut },
    { key: 'Mod-0', run: resetZoom }
  ]);
}

window.CodeMirrorBundle = {
  create(container, options = {}) {
    const { text = '', ext = '', dark = false, onDirty = null } = options;

    // Set while Rust is pushing a whole document into the view, so that the
    // programmatic dispatch does not get mistaken for a user edit.
    let loading = false;
    let destroyed = false;

    const instance = {
      view: null,
      // Left null on purpose: `applyZoom` skips an instance whose size already
      // matches, so seeding this with the current size meant the very first
      // apply was a no-op and the view kept whatever it inherited from the
      // stylesheet — which sets no font-size at all, leaving the browser's 16px.
      // Null guarantees the intended size is written inline on creation.
      fontSize: null,
      activeKey: null,
      activeDark: dark,
      // Live (possibly unsaved) text per tab, keyed by tab id.
      buffers: Object.create(null),

      getText() {
        return this.view.state.doc.toString();
      },

      /**
       * Point the editor at tab `key`.
       *
       * The first time a tab is shown its buffer is seeded from `diskText`; after
       * that the buffer wins, so switching tabs restores unsaved edits instead of
       * silently reverting them. `dirty` is reported by comparing the two.
       */
      load(key, diskText, ext, dark) {
        if (destroyed) return;
        let buf = this.buffers[key];
        if (buf === undefined) {
          buf = diskText;
          this.buffers[key] = buf;
        }
        if (this.activeKey !== key) {
          this.activeKey = key;
          loading = true;
          try {
            this.view.dispatch({
              changes: { from: 0, to: this.view.state.doc.length, insert: buf },
              effects: langCompartment.reconfigure(langExtension(ext))
            });
          } finally {
            loading = false;
          }
        }
        this.setTheme(dark);
        if (onDirty) onDirty(buf !== diskText);
      },

      setTheme(dark) {
        if (destroyed || this.activeDark === dark) return;
        this.activeDark = dark;
        this.view.dispatch({ effects: themeCompartment.reconfigure(themeExtension(dark)) });
      },

      /**
       * Entry point for the app shell (查看 menu) as well as the keymap, so
       * both routes land on the same shared zoom level. `delta === 0` resets.
       */
      changeFontSize(delta) {
        if (destroyed) return;
        if (delta > 0) stepZoom(delta);
        else if (delta < 0) stepZoom(delta);
        else resetZoom();
      },

      focus() {
        if (!destroyed) this.view.focus();
      },

      destroy() {
        if (destroyed) return;
        destroyed = true;
        liveInstances.delete(this);
        this.buffers = Object.create(null);
        this.activeKey = null;
        try { this.view.destroy(); } catch (e) {}
        this.view = null;
      }
    };

    instance.view = new EditorView({
      doc: text,
      extensions: [
        // Ahead of the bundled keymaps so zoom wins if one of them ever grows
        // its own Ctrl-+ binding.
        zoomKeymap(),
        keymap.of([
          ...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap
        ]),
        ...coreExtensions,
        langCompartment.of(langExtension(ext)),
        themeCompartment.of(themeExtension(dark)),
        EditorView.updateListener.of((update) => {
          if (destroyed || loading || !update.docChanged) return;
          // Mirror the document into the per-tab buffer. Without this the map
          // still holds whatever the file looked like when the tab was first
          // opened, so switching away and back — or saving a tab that is not
          // the one on screen — would silently use that stale copy.
          if (instance.activeKey !== null) {
            instance.buffers[instance.activeKey] = update.state.doc.toString();
          }
          if (onDirty) onDirty(true);
        })
      ],
      parent: container
    });

    // Register before applying so a fresh view picks up the shared zoom. The
    // stylesheet only supplies `--font-scale` and sets no `font-size` anywhere
    // in the editor's chain, so this inline write is what decides how large the
    // code actually is.
    liveInstances.add(instance);
    applyZoom();

    return instance;
  }
};