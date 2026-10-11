(function () {
  var TAB = $TAB, SHELL = '$SHELL';
  window.__terminals = window.__terminals || {};
  window.__maid_term = window.__maid_term || {};

  function post(m) {
    if (window.__maid_term.post) window.__maid_term.post(m);
  }

  // The app stylesheet keeps the UI type scale in --font-scale; reading it here
  // means one number drives the shell, the editor and the terminal together.
  var BASE_FS = 14;
  try {
    var scale = parseFloat(getComputedStyle(document.documentElement)
      .getPropertyValue('--font-scale'));
    if (scale > 0) BASE_FS = Math.round(14 * scale);
  } catch (x) {}

  function teardown(tab) {
    var e = window.__terminals[tab];
    if (e) {
      try { if (e.ro) e.ro.disconnect(); } catch (x) {}
      try { if (e.term) e.term.dispose(); } catch (x) {}
      delete window.__terminals[tab];
    }
  }

  function cellSize(term, container) {
    // xterm knows the rendered cell size once it has painted; before that fall
    // back to a DOM probe so the very first fit is not wrong.
    try {
      var d = term._core._renderService.dimensions;
      if (d && d.css && d.css.cell && d.css.cell.width > 0 && d.css.cell.height > 0) {
        return { w: d.css.cell.width, h: d.css.cell.height };
      }
    } catch (x) {}
    var probe = document.createElement('div');
    probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;line-height:normal;';
    probe.style.fontFamily = term.options.fontFamily;
    probe.style.fontSize = term.options.fontSize + 'px';
    probe.textContent = 'MMMMMMMMMM';
    container.appendChild(probe);
    var box = probe.getBoundingClientRect();
    container.removeChild(probe);
    var fs = term.options.fontSize || BASE_FS;
    return { w: (box.width / 10) || fs * 0.6, h: box.height || fs * 1.2 };
  }

  function create(attempt) {
    var container = document.getElementById('xterm-root-' + TAB);
    if (!container || typeof Terminal === 'undefined') {
      if (attempt < 80) setTimeout(function () { create(attempt + 1); }, 25);
      else post({ t: 'error', tab: TAB, message: !container ? '终端容器未加载' : 'xterm.js 未加载' });
      return;
    }
    teardown(TAB);
    container.innerHTML = '';

    var term;
    try {
      term = new Terminal({
        cursorBlink: true,
        fontSize: BASE_FS,
        fontFamily: "'JetBrains Mono','Cascadia Code','Fira Code','Consolas','Courier New',monospace",
        scrollback: 5000,
        theme: $THEME,
        allowTransparency: true,
        cursorStyle: 'bar'
      });
      term.open(container);
    } catch (e) {
      post({ t: 'error', tab: TAB, message: String(e) });
      return;
    }

    var entry = { term: term, ro: null, fontSize: BASE_FS, wasHidden: false };
    window.__terminals[TAB] = entry;

    function fit() {
      var w = container.clientWidth - 16;
      var h = container.clientHeight - 8;
      // Zero size means the pane is minimised: remember that, because when it
      // comes back the grid may be the exact same size and would otherwise skip
      // the resize that would have forced a repaint.
      if (!(w > 0) || !(h > 0)) { entry.wasHidden = true; return; }
      var cell = cellSize(term, container);
      if (!(cell.w > 0) || !(cell.h > 0)) return;
      var cols = Math.max(10, Math.min(500, Math.floor(w / cell.w)));
      var rows = Math.max(4, Math.min(200, Math.floor(h / cell.h)));
      if (cols === term.cols && rows === term.rows) {
        if (entry.wasHidden) {
          entry.wasHidden = false;
          try { term.refresh(0, rows - 1); } catch (x) {}
        }
        return;
      }
      entry.wasHidden = false;
      try { term.resize(cols, rows); } catch (x) { return; }
      post({ t: 'size', tab: TAB, c: cols, r: rows });
    }
    entry.fit = fit;

    term.onData(function (data) { post({ t: 'in', tab: TAB, d: data }); });
    if (term.parser && term.parser.registerOscHandler) {
      term.parser.registerOscHandler(7, function (data) {
        try {
          var uri = new URL(data);
          var path = decodeURIComponent(uri.pathname);
          if (uri.hostname) path = '\\\\' + uri.hostname + path.replace(/\//g, '\\\\');
          else if (/^\/[A-Za-z]:\//.test(path)) path = path.slice(1).replace(/\//g, '\\\\');
          post({ t: 'cwd', tab: TAB, d: path });
        } catch (e) {
          console.error('[Zmstop] 无法解析终端工作目录:', e);
        }
        return true;
      });
    }

    container.addEventListener('keydown', function (e) {
      if (!(e.ctrlKey || e.metaKey)) return;
      var delta = e.key === '=' || e.key === '+' ? 1 : (e.key === '-' ? -1 : 0);
      if (!delta) return;
      e.preventDefault();
      e.stopPropagation();
      entry.fontSize = Math.max(8, Math.min(40, entry.fontSize + delta));
      term.options.fontSize = entry.fontSize;
      fit();
    }, true);

    try {
      entry.ro = new ResizeObserver(function () { fit(); });
      entry.ro.observe(container);
    } catch (x) {}
    fit();
    term.focus();
    term.write('\x1b[1;36m终端 ' + (TAB + 1) + ' — ' + SHELL + '\x1b[0m\r\n');
  }

  create(0);
})();