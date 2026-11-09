<script lang="ts">
	// The share screen (PWA share target, Android): files shared from another app arrive here through
	// the service worker; the person picks whose they are and sends them to /api/ingest/share. It also
	// works as a plain file picker, so a phone without the share sheet can use it.
	import '#lib/components/pages2/pages2.css';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { shareFiles } from '#lib/api/inbox';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import { tf, tp } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { app, currentProfile } from '#lib/state/app.svelte';

	type Result = { filename: string; outcome?: string; duplicate?: boolean };
	let files = $state<File[]>([]);
	let profile = $state(currentProfile()?.slug ?? '');
	let waiting = $state(false);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let results = $state<Result[] | null>(null);
	const profiles = $derived(app.me?.profiles ?? []);
	$effect(() => {
		if (!profile && profiles.length) profile = currentProfile()?.slug ?? profiles[0].slug;
	});

	onMount(() => {
		if (!page.url.searchParams.has('received') || !('serviceWorker' in navigator)) return;
		waiting = true;
		const onMsg = (e: MessageEvent) => {
			if (e.data?.type !== 'share-files') return;
			files = [...files, ...(e.data.files as File[])];
			waiting = false;
		};
		navigator.serviceWorker.addEventListener('message', onMsg);
		navigator.serviceWorker.ready.then(() => navigator.serviceWorker.controller?.postMessage('share-ready'));
		const giveUp = setTimeout(() => (waiting = false), 8000);
		return () => {
			clearTimeout(giveUp);
			navigator.serviceWorker.removeEventListener('message', onMsg);
		};
	});

	function pick(e: Event) {
		const input = e.currentTarget as HTMLInputElement;
		files = [...files, ...Array.from(input.files ?? [])];
		input.value = '';
	}
	async function send() {
		busy = true;
		error = null;
		try {
			results = (await shareFiles(profile, files)).items;
			files = [];
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
	const size = (n: number) => (n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
	const whose = $derived(profiles.find((p) => p.slug === profile)?.display_name ?? '');
</script>

<PageHeader title={t('fu_share')} sub={t('fu_share_sub')} />

<div class="wrap">
	<section class="card p2form" aria-labelledby="sh-h">
		<h3 id="sh-h">{waiting ? t('fu_share_waiting') : files.length ? tp('fu_share_n', files.length) : t('fu_share_none')}</h3>
		{#if files.length}
			<ul class="files">
				{#each files as f, i (i)}
					<li>
						<span class="mono nm">{f.name}</span>
						<span class="dim">{size(f.size)}</span>
						<button class="btn sm" aria-label="{t('i2_remove')} {f.name}" onclick={() => (files = files.filter((_, j) => j !== i))}>×</button>
					</li>
				{/each}
			</ul>
		{/if}
		<label class="btn pick"><input type="file" multiple accept="application/pdf,image/*" onchange={pick} />{t('fu_share_pick')}</label>
		<label
			>{t('fu_share_whose')}
			<select class="p2in" bind:value={profile} required>
				{#each profiles as p (p.id)}<option value={p.slug}>{p.display_name}</option>{/each}
			</select>
		</label>
		<div class="note">{t('fu_share_note')}</div>
		{#if error}<div class="p2err" role="alert">{error}</div>{/if}
		<button class="btn pri big" disabled={busy || !files.length || !profile} onclick={send}>{whose ? tf('fu_share_send', { who: whose }) : t('fu_share_send0')}</button>
	</section>

	{#if results}
		<section class="card" aria-live="polite">
			<h3>{t('fu_share_done')}</h3>
			<ul class="files">
				{#each results as r, i (i)}
					<li><span class="mono nm">{r.filename}</span><span class="pill {r.duplicate || r.outcome === 'duplicate' || r.outcome === 'already_seen' ? 'p-stale' : 'p-ok'}"><i aria-hidden="true"></i>{r.duplicate || r.outcome === 'duplicate' || r.outcome === 'already_seen' ? t('fu_share_dup') : r.outcome === 'unsupported' ? t('fu_share_unsupported') : t('fu_share_queued')}</span></li>
				{/each}
			</ul>
			<a class="btn" href="/import-review">{t('nav_imp')}</a>
		</section>
	{/if}
</div>

<style>
	.wrap {
		max-width: 560px;
		display: grid;
		gap: 12px;
	}
	.files {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.files li {
		display: flex;
		gap: 8px;
		align-items: center;
		font-size: 12.5px;
	}
	.nm {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.pick {
		justify-content: center;
		cursor: pointer;
	}
	.pick input {
		position: absolute;
		width: 1px;
		height: 1px;
		opacity: 0;
	}
	.pick:focus-within {
		outline: 2px solid var(--accent);
	}
	.big {
		justify-content: center;
		padding: 9px 12px;
		font-size: 13px;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
