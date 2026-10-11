(function () {
  var tabs = window.__terminals || {};
  Object.keys(tabs).forEach(function (k) {
    var e = tabs[k];
    try { if (e.ro) e.ro.disconnect(); } catch (x) {}
    try { if (e.term) e.term.dispose(); } catch (x) {}
  });
  window.__terminals = {};
})();
