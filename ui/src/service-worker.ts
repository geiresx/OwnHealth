/// <reference types="@sveltejs/kit" />
/// <reference no-default-lib="true"/>
/// <reference lib="esnext" />
/// <reference lib="webworker" />
// The PWA service worker. It caches nothing: the record never sits in a browser cache
// (security-privacy.md 11, "the PWA caches only the ICE card offline, never the record"), and every
// request goes to the network as without it. Its one job is the Android share target: the manifest
// sends shared files as POST /share, which no server route answers, so the worker takes the form,
// sends the browser to the share screen, and hands the files over once that screen asks for them.
// The files live only in this worker's memory until then.

const sw = self as unknown as ServiceWorkerGlobalScope;
const HANDOFF_MS = 5 * 60 * 1000;

sw.addEventListener('install', () => {
	sw.skipWaiting();
});
sw.addEventListener('activate', (event) => {
	event.waitUntil(sw.clients.claim());
});

// The share screen posts 'share-ready' when it can take the files.
const ready = new Map<string, () => void>();
sw.addEventListener('message', (event) => {
	const source = event.source as Client | null;
	if (event.data === 'share-ready' && source) ready.get(source.id)?.();
});

function readyFrom(clientId: string): Promise<boolean> {
	return new Promise((resolve) => {
		const timer = setTimeout(() => {
			ready.delete(clientId);
			resolve(false);
		}, HANDOFF_MS);
		ready.set(clientId, () => {
			clearTimeout(timer);
			ready.delete(clientId);
			resolve(true);
		});
	});
}

sw.addEventListener('fetch', (event) => {
	const url = new URL(event.request.url);
	if (event.request.method !== 'POST' || url.origin !== sw.location.origin || url.pathname !== '/share') return;
	const form = event.request.formData();
	event.respondWith(Response.redirect('/share?received=1', 303));
	event.waitUntil(
		(async () => {
			const clientId = event.resultingClientId;
			if (!clientId || !(await readyFrom(clientId))) return;
			const client = await sw.clients.get(clientId);
			if (!client) return;
			const data = await form.catch(() => null);
			const files = (data?.getAll('files') ?? []).filter((f): f is File => f instanceof File && f.size > 0);
			const title = data?.get('title');
			client.postMessage({ type: 'share-files', files, title: typeof title === 'string' ? title : '' });
		})()
	);
});
