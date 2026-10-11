(function () {
  var tabs = window.__terminals || {};
  var e = tabs[$TAB];
  if (e) {
    try { if (e.ro) e.ro.disconnect(); } catch (x) {}
    try { if (e.term) e.term.dispose(); } catch (x) {}
    delete tabs[$TAB];
  }
})();
