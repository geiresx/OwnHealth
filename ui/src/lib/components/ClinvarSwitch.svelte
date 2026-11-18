<script lang="ts">
	// Settings: the household's weekly ClinVar check (source kind `clinvar`). On by itself once a genome
	// is imported; the switch turns it on or off, and a local file serves a server without internet.
	// It is one inbox source, so its health also shows on the Inbox screen.
	import { createInbox, listInboxes, updateInbox, type InboxSource } from '#lib/api/inbox';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import { ago } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { clinvarConfig, clinvarOn, sourceStatus } from '#lib/logic/inbox';
	import { app } from '#lib/state/app.svelte';

	// `canManage`: the reader owns or guards a profile; only then does the household's check show.
	let { canManage }: { canManage: boolean } = $props();

	const sources = new Loader<InboxSource[]>();
	const load = () => sources.run(async () => (await listInboxes()).items);
	$effect(() => {
		if (canManage) load();
	});
	const src = $derived(sources.data?.find((s) => s.kind === 'clinvar') ?? null);
	const demo = $derived(!!app.me?.demo);
	// With no source yet the switch shows the default the first genome brings (on, never in the demo).
	const on = $derived(clinvarOn(src, demo));
	// A mirror set through the API (config.url) stays when the path field is left empty.
	const mirror = $derived(typeof src?.config.url === 'string' ? src.config.url : '');
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	let saved = $state(false);
	let path = $state('');
	// The field follows the server until the person types in it.
	let typed = $state(false);
	$effect(() => {
		if (!typed) path = String(src?.config.path ?? '');
	});

	async function act(fn: () => Promise<unknown>) {
		busy = true;
		error = null;
		saved = false;
		try {
			await withStepUp(fn);
			return true;
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
			return false;
		} finally {
			// the field follows the server again only once the server's new row is here
			await load();
			busy = false;
		}
	}
	// The household has one check. The scheduler may have added it since this page loaded; the server
	// then refuses a second one, and the change goes to the row it added.
	async function upsert(change: { enabled?: boolean; config?: Record<string, unknown> }) {
		if (src) return updateInbox(src.id, change);
		try {
			return await createInbox({ kind: 'clinvar', label: 'ClinVar', config: change.config ?? {}, enabled: change.enabled ?? on });
		} catch (e) {
			const row = (await listInboxes()).items.find((s) => s.kind === 'clinvar');
			if (!row) throw e;
			return updateInbox(row.id, change);
		}
	}
	const toggle = (next: boolean) => act(() => upsert({ enabled: next }));
	async function save(e: SubmitEvent) {
		e.preventDefault();
		const p = path.trim();
		// The server checks the same rule; this says it in the reader's language before a round trip.
		if (p && !(p.startsWith('/') && /\.vcf(\.gz)?$/i.test(p))) {
			error = { text: t('gen_cv_set_bad'), raw: p };
			saved = false;
			return;
		}
		const config = clinvarConfig(p, src?.config);
		if (await act(() => upsert({ config }))) {
			saved = true;
			typed = false;
		}
	}
	const PILL = { ok: 'p-ok', out: 'p-out', partial: 'p-watch', stale: 'p-stale', nodata: 'p-stale', off: 'p-stale' } as const;
	const status = $derived(src ? sourceStatus(src) : null);
</script>

{#if canManage}
	<section class="card cvs" aria-labelledby="cvs-h">
		<h3 id="cvs-h">{t('gen_cv_set_t')}</h3>
		<div class="set">
			<Switch {on} label={t('gen_cv_set_t')} disabled={busy || !sources.data || !app.me} onchange={toggle} />
			<div class="dim" style="flex:1;min-width:0;font-size:11.5px">{t('gen_cv_set_d')}</div>
		</div>
		{#if sources.error}
			<NotReady error={sources.error} onretry={load} />
		{:else if src}
			<div class="state">
				<span class="dim">{t('gen_cv_set_last')}:</span>
				<span class="num">{src.last_run_at ? ago(src.last_run_at) : t('i2_never')}</span>
				{#if status && status !== 'off'}<span class="pill {PILL[status]}"><i aria-hidden="true"></i>{t('fu_st_' + status)}</span>{/if}
				<a href="/inbox">{t('fu_inbox')}</a>
			</div>
			{#if status === 'out' && src.last_error}<div class="err" role="status">{errText(new Error(src.last_error))}</div>{/if}
		{/if}
		<form class="path" onsubmit={save}>
			<label for="cvs-path" style="font-size:12px">{t('gen_cv_set_path')}</label>
			<div class="p2row">
				<input id="cvs-path" class="p2in mono" bind:value={path} disabled={busy} oninput={() => ((typed = true), (saved = false))} placeholder="/srv/clinvar/clinvar.vcf.gz" autocomplete="off" spellcheck="false" aria-describedby="cvs-path-d" aria-invalid={error?.raw === path.trim() ? true : undefined} />
				<button class="btn sm" type="submit" disabled={busy || !sources.data}>{t('fu_save')}</button>
			</div>
			<div class="dim" id="cvs-path-d" style="font-size:11px;overflow-wrap:anywhere">{t('gen_cv_set_path_d')} {mirror ? tf('gen_cv_set_mirror', { url: mirror }) : t('gen_cv_set_ncbi')}</div>
		</form>
		{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
		{#if saved}<div class="p2ok" role="status" style="font-size:12px">{t('gen_cv_set_saved')}</div>{/if}
	</section>
{/if}

<style>
	.cvs {
		display: grid;
		gap: 8px;
	}
	.cvs h3 {
		margin-bottom: 0;
	}
	.set {
		display: flex;
		gap: 10px;
		align-items: flex-start;
	}
	.state {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 8px;
		font-size: 12px;
	}
	.err {
		font-size: 11.5px;
		color: var(--out);
		overflow-wrap: anywhere;
	}
	.path {
		display: grid;
		gap: 4px;
	}
	.path .p2in {
		flex: 1;
		min-width: 0;
	}
</style>
