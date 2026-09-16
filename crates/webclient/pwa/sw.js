// Service worker: push, and the offline shell. Content-hashed assets are
// cached forever (a hash can be missing, never wrong); the shell and the
// stable-named scripts go network-first with a cached fallback, so a
// deploy lands the moment the network answers and hotel wifi or airplane
// mode still boots the app instead of a white screen. Live data — the
// API, the socket, /files — is never cached: a chat app must not show a
// stale conversation.
const CACHE = "nd-shell-v1";
const HASHED = /-dxh[a-z0-9]+\./;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) =>
  e.waitUntil(
    caches
      .keys()
      .then((names) => Promise.all(names.filter((n) => n !== CACHE).map((n) => caches.delete(n))))
      .then(() => self.clients.claim()),
  ),
);

// A fresh shell names the current build. When it changes, the previous
// build's hashed assets are dead weight — drop them, remember the new one.
function pruneOldBuild(html, cache) {
  const m = /webclient-(dxh[a-z0-9]+)\.js/.exec(html || "");
  if (!m) return;
  const build = m[1];
  cache.match("/app/__build").then((prev) =>
    (prev ? prev.text() : Promise.resolve("")).then((old) => {
      if (old === build) return;
      cache.put("/app/__build", new Response(build));
      // No previous marker means a first visit, and the only hashed
      // entries are the build being fetched right now — nothing stale.
      if (!old) return;
      cache.keys().then((keys) => {
        keys.forEach((k) => {
          if (HASHED.test(new URL(k.url).pathname)) cache.delete(k);
        });
      });
    }),
  );
}

self.addEventListener("fetch", (e) => {
  // Only GETs. A request this worker answers is one the browser makes on
  // the worker's behalf, and an upload that goes that way reports no
  // progress at all — the page's XMLHttpRequest sees 0% from start to
  // finish, which on a slow connection is indistinguishable from stuck
  // (Jon, with a 4 MB photo). Let uploads go straight to the network.
  if (e.request.method !== "GET") return;
  const url = new URL(e.request.url);
  const isNav = e.request.mode === "navigate";
  if (url.origin !== location.origin) return;
  // Only the app's own shell and assets; everything else passes through.
  if (!url.pathname.startsWith("/app")) return;
  if (!isNav && !url.pathname.startsWith("/app/")) return;

  if (HASHED.test(url.pathname)) {
    e.respondWith(
      caches.open(CACHE).then((c) =>
        c.match(e.request).then(
          (hit) =>
            hit ||
            fetch(e.request).then((r) => {
              if (r.ok) c.put(e.request, r.clone());
              return r;
            }),
        ),
      ),
    );
    return;
  }

  // Network first, cache as the answer when the network can't — or won't
  // within a few seconds, which is what makes "slow" behave like "offline"
  // instead of like a spinner. Every navigation serves and refreshes one
  // stored shell, so /app/?channel=12 boots offline too.
  const key = isNav ? "/app/" : e.request;
  e.respondWith(
    Promise.race([
      fetch(e.request),
      // Resolves a sentinel rather than rejecting: the loser of a race
      // still settles, and a stray rejection is console noise forever.
      new Promise((resolve) => setTimeout(resolve, isNav ? 3500 : 6000, "slow")),
    ])
      .then((r) => {
        if (r === "slow") throw r;
        if (r.ok) {
          const copy = r.clone();
          const scan = isNav ? r.clone() : null;
          caches.open(CACHE).then((c) => {
            c.put(key, copy);
            if (scan) scan.text().then((html) => pruneOldBuild(html, c)).catch(() => {});
          }).catch(() => {});
        }
        return r;
      })
      .catch(() =>
        caches.open(CACHE).then((c) => c.match(key).then((hit) => hit || fetch(e.request))),
      ),
  );
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
  // Whether this device should buzz is a question only this device can
  // answer, so the server no longer guesses: it sends, and we stay quiet if
  // one of our own windows is already open and in front of the user. Having
  // the desktop app running on another machine is not a reason for a phone
  // to say nothing.
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((windows) => {
        const looking = windows.some(
          (w) => w.visibilityState === "visible" && w.focused,
        );
        if (looking) return;
        return self.registration.showNotification(title, {
          body: data.body || "",
          icon: "/app/icon-192.png",
          badge: "/app/icon-192.png",
          // One notification per channel: a busy channel replaces rather
          // than stacking twenty times.
          tag: "nd-channel-" + (data.channel_id || "x"),
          renotify: true,
          data: { channel_id: data.channel_id, message_id: data.message_id },
        });
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
