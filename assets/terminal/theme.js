(function () {
  var theme = $THEME;
  var tabs = window.__terminals || {};
  Object.keys(tabs).forEach(function (k) {
    var e = tabs[k];
    if (e && e.term) e.term.options.theme = theme;
  });
})();
