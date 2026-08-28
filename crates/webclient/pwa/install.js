// Install glue. Chrome fires beforeinstallprompt once per page load and only
// when it feels like it — after an install-then-uninstall it can stay quiet
// for months, which leaves people staring at a browser that has silently
// decided they've already been asked. Catching the event ourselves means the
// app can always offer a button, and say what to do when it can't.
window.ndInstall = (() => {
  const state = { available: false, installed: false, ios: false, busy: false, outcome: "" };
  // The page's first inline script starts catching the event before
  // this file is even fetched; pick up anything it already has.
  let deferred = window.__ndInstallEvent || null;

  function refresh() {
    const standalone =
      window.matchMedia("(display-mode: standalone)").matches ||
      window.matchMedia("(display-mode: window-controls-overlay)").matches ||
      // iOS Safari's own flag; it has no display-mode support worth trusting.
      window.navigator.standalone === true;
    state.installed = standalone;
    // iOS never fires beforeinstallprompt — Share → Add to Home Screen is the
    // only route, so the app tells people that instead of showing a button.
    state.ios = /iPad|iPhone|iPod/.test(navigator.userAgent) && !window.MSStream;
    state.available = deferred !== null;
  }
  refresh();

  window.addEventListener("beforeinstallprompt", (e) => {
    // Suppress the mini-infobar so ours is the only prompt, and keep the
    // event: it's the one object that can open the real install dialog.
    e.preventDefault();
    deferred = e;
    window.__ndInstallEvent = e;
    state.available = true;
  });

  window.addEventListener("appinstalled", () => {
    deferred = null;
    window.__ndInstallEvent = null;
    state.available = false;
    state.installed = true;
    state.outcome = "installed";
  });

  async function prompt() {
    if (!deferred) return "unavailable";
    state.busy = true;
    try {
      deferred.prompt();
      const choice = await deferred.userChoice;
      // The event is single-use whichever way the answer went.
      deferred = null;
      window.__ndInstallEvent = null;
      state.available = false;
      state.outcome = choice && choice.outcome === "accepted" ? "installed" : "dismissed";
      return state.outcome;
    } catch (e) {
      state.outcome = String(e && e.message ? e.message : e);
      return state.outcome;
    } finally {
      state.busy = false;
    }
  }

  function getState() {
    refresh();
    return JSON.stringify(state);
  }

  return { prompt, getState };
})();
