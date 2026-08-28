// Minimal service worker: enough for installability, deliberately no caching
// beyond the shell fallback — a chat app should never show stale data, and
// the wasm/js assets are content-hashed so the browser cache handles them.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (e) => {
  e.respondWith(fetch(e.request));
});
