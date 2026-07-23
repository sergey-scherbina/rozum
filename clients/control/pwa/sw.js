// Self-destruct service worker.
// The former no-op "network-only" SW gave no caching/offline benefit and could leave iOS
// standalone PWAs blank once it claimed the client. This version unregisters itself, clears any
// caches, and reloads controlled windows once — so a phone that still has the old worker recovers
// on the next open. control-serve no longer injects a registration, so it does not come back.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    try {
      const keys = await caches.keys();
      await Promise.all(keys.map((k) => caches.delete(k)));
    } catch (e) {}
    try { await self.registration.unregister(); } catch (e) {}
    try {
      const clients = await self.clients.matchAll({ type: 'window' });
      clients.forEach((c) => { try { c.navigate(c.url); } catch (e) {} });
    } catch (e) {}
  })());
});
