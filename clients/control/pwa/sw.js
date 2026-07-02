// Auto-update service worker: skipWaiting on install so new version activates
// immediately without the user having to close and reopen the PWA.
// No caching — all requests go to the network (HTTP no-store on HTML handles caching).

self.addEventListener('install', () => self.skipWaiting());

self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));
