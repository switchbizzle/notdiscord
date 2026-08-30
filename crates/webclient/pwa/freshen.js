// An installed PWA keeps the page loaded for days. Nothing reloads it, so a
// deploy reaches the server and the phone carries on running whatever it
// booted with — which is how switchb ended up unable to see a typing
// indicator that had already shipped.
//
// The assets are content-hashed, so a new build means a new filename in
// index.html. Whenever the app comes back to the foreground, re-fetch the
// shell and compare: a different bundle means reload.
(function () {
  // Whatever this page actually loaded, straight from the DOM — no build
  // constant to forget to bump.
  var mine = null;
  var scripts = document.querySelectorAll('script[src]');
  for (var i = 0; i < scripts.length; i++) {
    var m = /(webclient-[A-Za-z0-9]+\.js)/.exec(scripts[i].getAttribute("src") || "");
    if (m) {
      mine = m[1];
      break;
    }
  }
  if (!mine) return;

  // Don't hammer the server on every tab switch.
  var MIN_GAP_MS = 60 * 1000;
  var last = 0;
  var reloading = false;

  function check() {
    if (reloading || document.visibilityState !== "visible") return;
    var now = Date.now();
    if (now - last < MIN_GAP_MS) return;
    last = now;
    fetch("./", { cache: "no-store" })
      .then(function (r) {
        return r.ok ? r.text() : null;
      })
      .then(function (html) {
        if (!html) return;
        var m = /(webclient-[A-Za-z0-9]+\.js)/.exec(html);
        if (!m || m[1] === mine) return;
        // A newer build is on the server. Reloading loses nothing: the draft
        // lives in the composer for a moment, and everything else is on the
        // server anyway.
        reloading = true;
        location.reload();
      })
      .catch(function () {
        /* offline, or the server is restarting — try again next time */
      });
  }

  document.addEventListener("visibilitychange", check);
  window.addEventListener("focus", check);
  // And once an hour for a phone that simply stays open.
  setInterval(check, 60 * 60 * 1000);
})();
