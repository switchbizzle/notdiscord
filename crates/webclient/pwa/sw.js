// Minimal service worker: enough for installability, deliberately no caching
// beyond the shell fallback — a chat app should never show stale data, and
// the wasm/js assets are content-hashed so the browser cache handles them.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (e) => {
  e.respondWith(fetch(e.request));
});

// A push arrives even when the app isn't running — that's the whole point.
self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch (e) {
    data = { title: "NotDiscord", body: event.data ? event.data.text() : "" };
  }
  const title = data.title || "NotDiscord";
  event.waitUntil(
    self.registration.showNotification(title, {
      body: data.body || "",
      icon: "/app/icon-192.png",
      badge: "/app/icon-192.png",
      // One notification per channel: a busy channel replaces rather than
      // stacking twenty times.
      tag: "nd-channel-" + (data.channel_id || "x"),
      renotify: true,
      data: { channel_id: data.channel_id, message_id: data.message_id },
    }),
  );
});

// Tapping it should land you in the right channel, reusing an open tab.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = "/app/?channel=" + (event.notification.data?.channel_id ?? "");
  event.waitUntil(
    clients.matchAll({ type: "window", includeUncontrolled: true }).then((list) => {
      for (const client of list) {
        if (client.url.includes("/app") && "focus" in client) {
          client.postMessage({ type: "open-channel", channel_id: event.notification.data?.channel_id });
          return client.focus();
        }
      }
      return clients.openWindow(target);
    }),
  );
});
