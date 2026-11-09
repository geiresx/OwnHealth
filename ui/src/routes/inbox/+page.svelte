<script lang="ts">
	// The Inbox screen (v0.2): every configured source with its status, last run and errors; the
	// quarantine of mail from senders not on the allowlist, with "release" and "release and allow
	// sender". Held mail is never read, classified or indexed until a person releases it.
	import '#lib/components/pages2/pages2.css';
	import {
		deleteInbox,
		deleteQuarantined,
		issueInboxToken,
		listInboxes,
		listQuarantine,
		releaseQuarantined,
		syncInbox,
		updateInbox,
		type InboxSource,
		type QuarantineOut
	} from '#lib/api/inbox';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import { ago, plain } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import SourceCard from '#lib/components/inbox/SourceCard.svelte';
	import SourceForm from '#lib/components/inbox/SourceForm.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { RECIPE, RECIPES_URL, heldTotal } from '#lib/logic/inbox';
	import { app, currentProfile } from '#lib/state/app.svelte';

	const sources = new Loader<InboxSource[]>();
	const held = new Loader<QuarantineOut[]>();
	const load = () => {
		sources.run(async () => (await listInboxes()).items);
		held.run(async () => (await listQuarantine('held')).items);
	};
	$effect(load);

	const profiles = $derived(app.me?.profiles ?? []);
	let form = $state<null | 'new' | InboxSource>(null);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let fresh = $state<{ source: InboxSource; token: string } | null>(null);
	let copied = $state(false);
	// Inline release form: which held message, into which profile.
	let releasing = $state<string | null>(null);
	let target = $state('');
	// A held message waiting for "delete for good" (one click alone deletes nothing).
	let deleting = $state<string | null>(null);

	async function act(fn: () => Promise<unknown>, done?: string) {
		busy = true;
		error = null;
		notice = null;
		try {
			await withStepUp(fn);
			if (done) notice = done;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
			load();
		}
	}
	const token = (s: InboxSource) =>
		act(async () => {
			const r = await issueInboxToken(s.id);
			fresh = { source: s, token: r.token };
			copied = false;
		});
	const remove = (q: QuarantineOut) =>
		act(async () => {
			await deleteQuarantined(q.id);
			deleting = null;
		});
	function startRelease(q: QuarantineOut) {
		releasing = releasing === q.id ? null : q.id;
		target = currentProfile()?.id ?? profiles[0]?.id ?? '';
	}
	const release = (q: QuarantineOut, allow: boolean) =>
		act(async () => {
			const r = await releaseQuarantined(q.id, { profile_id: target, allow_sender: allow });
			releasing = null;
			notice = tf(allow ? 'fu_q_released_allowed' : 'fu_q_released', { n: r.document_ids.length, sender: plain(q.sender) });
			if (r.problems?.length) notice += ' ' + tp('fu_q_released_skipped', r.problems.length);
		});
	async function copy() {
		if (!fresh) return;
		await navigator.clipboard?.writeText(fresh.token).catch(() => null);
		copied = true;
	}
	// Where each push source sends, with which header (connectors.md, ingestion.md 3).
	const PUSH: Record<string, { path: string; header: string }> = {
		paperless: { path: '/api/ingest/paperless', header: 'X-OwnHealth-Secret: ' },
		shortcut: { path: '/api/ingest/shortcut', header: 'Authorization: Bearer ' },
		health_auto_export: { path: '/api/ingest/health-auto-export', header: 'Authorization: Bearer ' }
	};
	const origin = typeof location !== 'undefined' ? location.origin : '';
	const sourceName = (id: string | null) => sources.data?.find((s) => s.id === id)?.label ?? '—';
	const doneFolder = (id: string | null) => String(sources.data?.find((s) => s.id === id)?.config.done_folder ?? 'OwnHealth/Done');
	const failing = $derived((sources.data ?? []).filter((s) => s.enabled && s.consecutive_failures > 0).length);
	// the server's count, not the length of the first page of the list
	const heldCount = $derived(heldTotal(sources.data));
	const lastSync = $derived(
		(sources.data ?? [])
			.map((s) => s.last_success_at)
			.filter((x): x is string => !!x)
			.sort()
			.at(-1) ?? null
	);
	const kb = (n: number) => (n >= 1048576 ? `${(n / 1048576).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
</script>

<PageHeader title={t('fu_inbox')} sub={t('fu_inbox_sub')}>
	{#snippet right()}
		<PhaseChip phase="v0.2" />
		<a class="btn" href="/import-review">{t('nav_imp')}</a>
		<button class="btn pri" onclick={() => (form = form === 'new' ? null : 'new')} aria-expanded={form === 'new'}>{t('fu_add')}</button>
	{/snippet}
</PageHeader>

<div class="inbox">
	<div class="ib"><div><div class="k">{t('fu_sources')}</div><div class="dim d">{t('fu_sources_d')}</div></div><span class="c">{sources.data?.length ?? '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_failing')}</div><div class="dim d">{t('fu_failing_d')}</div></div><span class="c" style="color:{failing ? 'var(--out)' : 'var(--dim)'}">{sources.data ? failing : '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_quarantine')}</div><div class="dim d">{t('fu_quarantine_d')}</div></div><span class="c" style="color:{heldCount ? 'var(--watch)' : 'var(--dim)'}">{heldCount ?? '—'}</span></div>
	<div class="ib"><div><div class="k">{t('fu_last_ok')}</div><div class="dim d">{t('fu_last_ok_d')}</div></div><span class="c" style="font-size:12.5px;font-weight:560">{lastSync ? ago(lastSync) : t('i2_never')}</span></div>
</div>

{#if error}<div class="p2err" role="alert" style="margin-bottom:8px">{error}</div>{/if}
{#if notice}<div class="p2ok" role="status" style="margin-bottom:8px;font-size:12px">{notice}</div>{/if}

{#if form}
	{#key form}
		<SourceForm
			{profiles}
			source={form === 'new' ? null : form}
			oncancel={() => (form = null)}
			onsaved={(s) => {
				const added = form === 'new';
				form = null;
				load();
				if (added && ['paperless', 'shortcut', 'health_auto_export'].includes(s.kind)) token(s);
			}}
		/>
	{/key}
{/if}

{#if fresh}
	<section class="card tok" aria-live="polite">
		<h3>{tf('fu_token_made', { label: fresh.source.label })}<span class="r"><button class="btn sm" onclick={copy}>{copied ? t('i2_copied') : t('i2_copy')}</button> <button class="btn sm" onclick={() => (fresh = null)}>{t('fu_done')}</button></span></h3>
		<div class="p2secret">{fresh.token}</div>
		{#if PUSH[fresh.source.kind]}
			<pre class="code">POST {origin}{PUSH[fresh.source.kind].path}
{PUSH[fresh.source.kind].header}{fresh.token.slice(0, 12)}…</pre>
		{/if}
		<div class="note">
			{t('fu_token_once')} {t('fu_tk_' + fresh.source.kind)}
			{#if RECIPE[fresh.source.kind]}<a href={RECIPES_URL + RECIPE[fresh.source.kind]} target="_blank" rel="noopener noreferrer">{t('fu_recipe')} ↗</a>{/if}
		</div>
	</section>
{/if}

{#if sources.error}
	<NotReady error={sources.error} onretry={load} />
{:else if sources.loading && !sources.data}
	<div class="empty">{t('loading')}</div>
{:else if !sources.data?.length}
	<div class="card"><div class="empty">{t('fu_none')} <button class="btn sm pri" onclick={() => (form = 'new')}>{t('fu_add')}</button></div></div>
{:else}
	<div class="srcs">
		{#each sources.data as s (s.id)}
			<SourceCard
				source={s}
				{profiles}
				{busy}
				onsync={() => act(() => syncInbox(s.id), tf('fu_sync_queued', { label: s.label }))}
				ontoggle={(on) => act(() => updateInbox(s.id, { enabled: on }))}
				onedit={() => (form = s)}
				ondelete={() => act(() => deleteInbox(s.id))}
				ontoken={() => token(s)}
				onchange={load}
			/>
		{/each}
	</div>
{/if}

<section class="card q" aria-labelledby="q-h">
	<h3 id="q-h">{t('fu_quarantine')}<span class="r">{t('fu_q_r')}</span></h3>
	{#if held.error}
		<NotReady error={held.error} onretry={load} />
	{:else if held.loading && !held.data}
		<div class="empty">{t('loading')}</div>
	{:else if !held.data?.length}
		<div class="empty">{t('fu_q_none')}</div>
	{:else}
		<div style="overflow-x:auto">
			<table class="t">
				<thead>
					<tr><th>{t('fu_q_received')}</th><th>{t('fu_q_from')}</th><th>{t('fu_q_subject')}</th><th>{t('fu_q_files')}</th><th>{t('fu_q_reason')}</th><th></th></tr>
				</thead>
				<tbody>
					{#each held.data as q (q.id)}
						<tr>
							<td class="dim num nw">{ago(q.received_at)}<div style="font-size:10.5px">{sourceName(q.source_id)}</div></td>
							<td class="mono ut">{plain(q.sender) || '—'}</td>
							<td class="ut"><span title={t('fu_untrusted_title')}>{plain(q.subject) || t('fu_q_nosubject')}</span></td>
							<td>
								{#each q.attachments as a, i (i)}
									<div class="att"><span class="mono">{plain(a.filename)}</span> <span class="dim">{kb(a.size)}</span></div>
								{:else}<span class="dim">{t('fu_q_nofiles')}</span>{/each}
							</td>
							<td><span class="pill {q.reason === 'auth_failed' ? 'p-out' : 'p-watch'}"><i aria-hidden="true"></i>{t('fu_qr_' + q.reason)}</span></td>
							<td class="nw" style="text-align:right">
								{#if q.reason !== 'too_large'}
									<button class="btn sm" onclick={() => startRelease(q)} aria-expanded={releasing === q.id}>{t('fu_q_release')}</button>
								{/if}
								{#if deleting === q.id}
									<button class="btn sm no" disabled={busy} onclick={() => remove(q)}>{t('fu_delete_yes')}</button>
									<button class="btn sm" onclick={() => (deleting = null)}>{t('i2_cancel')}</button>
								{:else}
									<button class="btn sm no" disabled={busy} onclick={() => (deleting = q.id)}>{t('fu_delete')}</button>
								{/if}
							</td>
						</tr>
						{#if q.reason === 'too_large'}
							<tr class="rel">
								<td colspan="6"><div class="note">{tf('fu_q_too_large_d', { folder: doneFolder(q.source_id) })}</div></td>
							</tr>
						{/if}
						{#if releasing === q.id}
							<tr class="rel">
								<td colspan="6">
									<div class="p2row">
										<span class="muted" style="font-size:12px">{t('fu_q_into')}</span>
										<select class="p2in" bind:value={target} aria-label={t('fu_f_profile')}>
											{#each profiles as p (p.id)}<option value={p.id}>{p.display_name}</option>{/each}
										</select>
										<button class="btn sm" disabled={busy || !target} onclick={() => release(q, false)}>{t('fu_q_once')}</button>
										<button class="btn sm pri" disabled={busy || !target || q.reason === 'auth_failed'} onclick={() => release(q, true)} title={q.reason === 'auth_failed' ? t('fu_q_auth_no_allow') : ''}>{t('fu_q_allow')}</button>
										<button class="btn sm" onclick={() => (releasing = null)}>{t('i2_cancel')}</button>
									</div>
									<div class="note" style="margin-top:4px">{t('fu_q_release_d')}</div>
								</td>
							</tr>
						{/if}
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<div class="note" style="margin-top:8px">
		<span class="chip" style="color:var(--out);border-color:rgba(239,107,115,.5)" title={t('fu_untrusted_title')}>{t('fu_untrusted_chip')}</span>
		{t('fu_q_note')}
	</div>
</section>

<style>
	.inbox .k {
		font-weight: 600;
		font-size: 12.5px;
	}
	.inbox .d {
		font-size: 11px;
	}
	.srcs {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(330px, 1fr));
		gap: 12px;
		margin-bottom: 12px;
	}
	.tok {
		border-color: rgba(224, 168, 74, 0.55);
		margin-bottom: 12px;
		display: grid;
		gap: 8px;
	}
	.q .ut {
		max-width: 320px;
		overflow-wrap: anywhere;
		color: var(--muted);
	}
	.att {
		font-size: 11.5px;
		white-space: nowrap;
	}
	.nw {
		white-space: nowrap;
	}
	tr.rel td {
		background: var(--raised);
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
	@media (max-width: 760px) {
		.inbox {
			flex-wrap: wrap;
		}
		.inbox .ib {
			flex: 1 1 45%;
		}
		.srcs {
			grid-template-columns: 1fr;
		}
	}
</style>
