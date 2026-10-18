// Caches the app shell so the PWA opens instantly; API calls always go to the daemon.
const SHELL = ['/', '/app.js', '/style.css', '/icon.svg', '/manifest.webmanifest'];
self.addEventListener('install', (e) => e.waitUntil(caches.open('fleet51-v1').then((c) => c.addAll(SHELL))));
self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (url.pathname.startsWith('/api/')) return;
  e.respondWith(fetch(e.request).catch(() => caches.match(e.request)));
});
