(function () {
  var tabs = window.__terminals || {};
  Object.keys(tabs).forEach(function (k) {
    var el = document.getElementById('xterm-root-' + k);
    if (el) el.classList.toggle('active', Number(k) === $TAB);
  });
  var e = tabs[$TAB];
  if (e) {
    requestAnimationFrame(function () {
      try { if (e.fit) e.fit(); e.term.focus(); } catch (x) {}
    });
  }
})();
