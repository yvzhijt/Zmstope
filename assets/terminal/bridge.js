window.__maid_term = window.__maid_term || {};
window.__maid_term.ch = dioxus;
window.__maid_term.post = function (msg) {
  try { window.__maid_term.ch.send(msg); } catch (e) {}
};
if (!window.__maid_term.dragInstalled) {
  window.__maid_term.dragInstalled = true;
  var dragging = false, startY = 0, startH = 0;
  // Seeded from the same constant Rust uses, so the drag can never start from a
  // stale height left behind by an earlier session in this webview.
  window.__maid_term.height = window.__maid_term.height || $DEFAULT_H;
  document.addEventListener('pointerdown', function (e) {
    var el = e.target;
    if (!el || !el.classList || !el.classList.contains('terminal-resize-handle')) return;
    dragging = true;
    startY = e.pageY;
    startH = window.__maid_term.height;
    // Suppress text selection and keep the resize cursor for the whole drag,
    // even when the pointer wanders off the six pixel handle.
    document.body.classList.add('term-dragging');
    e.preventDefault();
  }, true);
  document.addEventListener('pointermove', function (e) {
    if (!dragging) return;
    // The panel is docked at the bottom, so dragging up grows it. The ceiling is
    // the window minus the room the editor keeps: a fixed pixel maximum would
    // swallow the editor on a short window.
    var ceiling = Math.max($MIN_H, window.innerHeight - $MIN_EDITOR_H);
    var h = Math.round(Math.max($MIN_H, Math.min(ceiling, startH + (startY - e.pageY))));
    if (h !== window.__maid_term.height) {
      window.__maid_term.height = h;
      window.__maid_term.post({ t: 'h', v: h });
    }
    e.preventDefault();
  }, { passive: false });
  function endDrag() {
    if (!dragging) return;
    dragging = false;
    document.body.classList.remove('term-dragging');
  }
  document.addEventListener('pointerup', endDrag, true);
  document.addEventListener('pointercancel', endDrag, true);
}
window.__maid_term.post({ t: 'ready' });
