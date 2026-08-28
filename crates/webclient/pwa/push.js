// Push subscription glue. Like voice.js, the wasm app calls in and polls
// the result back out as JSON — nothing async crosses the boundary.
window.ndPush = (() => {
  const state = { supported: false, permission: "default", endpoint: "", busy: false, error: "" };

  function refresh() {
    state.supported = "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
    state.permission = state.supported ? Notification.permission : "unsupported";
  }
  refresh();

  function urlB64ToUint8Array(base64String) {
    const padding = "=".repeat((4 - (base64String.length % 4)) % 4);
    const base64 = (base64String + padding).replace(/-/g, "+").replace(/_/g, "/");
    const raw = atob(base64);
    return Uint8Array.from([...raw].map((c) => c.charCodeAt(0)));
  }

  function keysOf(sub) {
    const raw = sub.toJSON().keys || {};
    return { endpoint: sub.endpoint, p256dh: raw.p256dh || "", auth: raw.auth || "" };
  }

  // serviceWorker.ready never resolves when no worker takes control (a
  // failed registration, a browser with SWs disabled), so every caller
  // races it — a hung promise would freeze the settings UI in its
  // "still loading" state forever.
  function readyOrNull(ms) {
    if (!("serviceWorker" in navigator)) return Promise.resolve(null);
    return Promise.race([
      navigator.serviceWorker.ready,
      new Promise((resolve) => setTimeout(() => resolve(null), ms)),
    ]);
  }

  // Report the existing subscription (if any) so the UI opens in the right
  // state on a device that already said yes.
  async function current() {
    refresh();
    if (!state.supported) return "";
    try {
      const reg = await readyOrNull(3000);
      if (!reg) {
        state.error = "notifications need the app installed to your Home Screen";
        return "";
      }
      const sub = await reg.pushManager.getSubscription();
      state.endpoint = sub ? sub.endpoint : "";
      return sub ? JSON.stringify(keysOf(sub)) : "";
    } catch (e) {
      state.error = String(e && e.message ? e.message : e);
      return "";
    }
  }

  // Ask permission and subscribe. Returns the subscription JSON for the app
  // to register with the server, or "" if the user said no.
  async function enable(vapidKey) {
    refresh();
    if (!state.supported) {
      state.error = "this browser can't do notifications";
      return "";
    }
    state.busy = true;
    state.error = "";
    try {
      const permission = await Notification.requestPermission();
      state.permission = permission;
      if (permission !== "granted") {
        state.error = permission === "denied" ? "notifications are blocked for this site" : "";
        return "";
      }
      const reg = await readyOrNull(5000);
      if (!reg) {
        state.error = "the app isn't installed on this device yet";
        return "";
      }
      let sub = await reg.pushManager.getSubscription();
      if (!sub) {
        sub = await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlB64ToUint8Array(vapidKey),
        });
      }
      state.endpoint = sub.endpoint;
      return JSON.stringify(keysOf(sub));
    } catch (e) {
      state.error = String(e && e.message ? e.message : e);
      return "";
    } finally {
      state.busy = false;
    }
  }

  async function disable() {
    try {
      const reg = await readyOrNull(3000);
      if (!reg) return "";
      const sub = await reg.pushManager.getSubscription();
      if (!sub) return "";
      const info = JSON.stringify(keysOf(sub));
      await sub.unsubscribe();
      state.endpoint = "";
      return info;
    } catch (e) {
      state.error = String(e && e.message ? e.message : e);
      return "";
    }
  }

  function getState() {
    refresh();
    return JSON.stringify(state);
  }

  return { current, enable, disable, getState };
})();
