import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig, loadEnv } from 'vite';

// The built UI lands in server/static: the release binary embeds it (rust-embed), and a debug
// server reads it from there.
const out = '../server/static';
// `pnpm dev` proxies the API and MCP to a server on :8080 (`cargo run -p ownhealth -- serve`).
const backend = loadEnv('', '.', 'OWNHEALTH_').OWNHEALTH_BACKEND ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [sveltekit({ adapter: adapter({ pages: out, assets: out, fallback: 'index.html', strict: true }) })],
	server: { proxy: { '/api': backend, '/mcp': backend, '/healthz': backend } }
});
