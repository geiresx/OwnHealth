<script lang="ts">
	// One inbox on the Inbox screen: its status (OK, Out, Partial, Stale, No data, Off), last run, last successful
	// sync, the last error, and its actions. Push sources get a token (shown once by the parent).
	import { listRuns, type InboxSource, type SourceRun } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import { Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import { ago, stamp } from '#lib/components/pages2/fmt';
	import { t } from '#lib/i18n/index.svelte';
	import { POLL_KINDS, PUSH_KINDS, sourceStatus } from '#lib/logic/inbox';
	import SenderRules from './SenderRules.svelte';

	let {
		source,
		profiles,
		busy = false,
		onsync,
		ontoggle,
		onedit,
		ondelete,
		ontoken,
		onchange
	}: {
		source: InboxSource;
		profiles: ProfileSummary[];
		busy?: boolean;
		onsync: () => void;
		ontoggle: (on: boolean) => void;
		onedit: () => void;
		ondelete: () => void;
		ontoken: () => void;
		onchange: () => void;
	} = $props();

	const PILL = { ok: 'p-ok', out: 'p-out', partial: 'p-watch', stale: 'p-stale', nodata: 'p-stale', off: 'p-stale' } as const;
	const status = $derived(sourceStatus(source));
	const runs = new Loader<SourceRun[]>();
	let open = $state<'' | 'runs' | 'senders'>('');
	let confirming = $state(false);
	function toggle(what: 'runs' | 'senders') {
		open = open === what ? '' : what;
		if (open === 'runs') runs.run(async () => (await listRuns(source.id, 10)).items, source.id);
	}
	const c = $derived(source.config as Record<string, string | number | boolean | null | undefined>);
	const detail = $derived.by(() => {
		switch (source.kind) {
			case 'email':
				return [c.username, c.host].filter(Boolean).join(' · ');
			case 'folder':
				return `OWNHEALTH_INBOX_DIR/${c.path ? c.path + '/' : ''}`;
			case 'paperless':
				return [c.url, c.tag ? `tag: ${c.tag}` : null].filter(Boolean).join(' · ');
			case 'nightscout':
				return String(c.url ?? '');
			default:
				return source.token_prefix ? `${source.token_prefix}…` : t('fu_no_token');
		}
	});
	const who = $derived(
		source.profile_id
			? (profiles.find((p) => p.id === source.profile_id)?.display_name ?? '—')
			: source.kind === 'folder'
				? t('fu_f_profile_folder')
				: t('fu_f_profile_rule')
	);
	const runTone = { ok: 'p-ok', partial: 'p-watch', error: 'p-out', running: 'p-info' } as const;
</script>

<!-- An open panel (runs, senders) takes the whole row, so its table never spills into the next card. -->
<article class="card srccard" class:wide={open !== ''} aria-labelledby="src-{source.id}">
	<div class="hd">
		<div style="min-width:0">
			<b id="src-{source.id}">{source.label}</b>
			<div class="dim kind">{t('fu_k_' + source.kind)} · <span class="mono">{detail}</span></div>
		</div>
		<span class="pill {PILL[status]}"><i aria-hidden="true"></i>{t('fu_st_' + status)}</span>
	</div>
	<dl class="facts">
		<dt>{t('fu_to')}</dt>
		<dd>{who}</dd>
		<dt>{t('fu_last_run')}</dt>
		<dd>{source.last_run_at ? ago(source.last_run_at) : t('i2_never')}</dd>
		<dt>{t('fu_last_ok')}</dt>
		<dd class:stale={source.stale}>{source.last_success_at ? ago(source.last_success_at) : t('i2_never')}</dd>
		{#if source.kind === 'email' && source.quarantined}
			<dt>{t('fu_quarantine')}</dt>
			<dd style="color:var(--watch)">{source.quarantined}</dd>
		{/if}
	</dl>
	{#if status === 'out' && source.last_error}
		<div class="err" role="status">⚠ {tp('fu_failed_n', source.consecutive_failures)} <span class="mono">{source.last_error}</span></div>
	{:else if status === 'partial'}
		<div class="note" role="status" style="color:var(--watch);overflow-wrap:anywhere">
			⚠ {tp('fu_partial_n', source.last_run_problems ?? 0)}
			{#if source.last_run_problem}<span class="mono">{source.last_run_problem}</span>{/if}
		</div>
	{:else if status === 'stale'}
		<div class="note" style="color:var(--stale)">{t('fu_stale_d')}</div>
	{/if}
	<div class="acts">
		<Switch on={source.enabled} label={tf('fu_enabled', { label: source.label })} disabled={busy} onchange={ontoggle} />
		{#if POLL_KINDS.includes(source.kind)}<button class="btn sm" disabled={busy || !source.enabled} onclick={onsync}>{t('fu_sync')}</button>{/if}
		{#if PUSH_KINDS.includes(source.kind)}<button class="btn sm" disabled={busy} onclick={ontoken}>{source.token_prefix ? t('fu_token_new') : t('fu_token')}</button>{/if}
		{#if source.kind === 'email'}<button class="btn sm" onclick={() => toggle('senders')} aria-expanded={open === 'senders'}>{t('fu_senders')}</button>{/if}
		<button class="btn sm" onclick={() => toggle('runs')} aria-expanded={open === 'runs'}>{t('fu_runs')}</button>
		<button class="btn sm" onclick={onedit}>{t('i2_edit')}</button>
		{#if confirming}
			<button class="btn sm no" disabled={busy} onclick={ondelete}>{t('fu_delete_yes')}</button>
			<button class="btn sm" onclick={() => (confirming = false)}>{t('i2_cancel')}</button>
		{:else}
			<button class="btn sm no" onclick={() => (confirming = true)}>{t('fu_delete')}</button>
		{/if}
	</div>
	{#if open === 'runs'}
		<div class="runs">
			{#if runs.error}
				<div class="p2err">{runs.error.message}</div>
			{:else if runs.loading && !runs.data}
				<div class="dim">{t('loading')}</div>
			{:else}
				<table class="t nw">
					<tbody>
						{#each runs.data ?? [] as r (r.id)}
							<tr>
								<td class="num dim">{stamp(r.started_at)}</td>
								<td class="dim">{t('fu_trig_' + r.trigger)}</td>
								<td><span class="pill {runTone[r.status]}"><i aria-hidden="true"></i>{t('fu_run_' + r.status)}</span></td>
								<td class="num">{tf('fu_run_counts', { seen: r.items_seen, new: r.items_new, dup: r.items_duplicate, q: r.items_quarantined })}</td>
							</tr>
							{#if r.error || r.errors.length}
								<tr><td colspan="4" class="mono err2">{[r.error, ...r.errors.map((e) => `${e.code}: ${e.message}`)].filter(Boolean).join(' · ')}</td></tr>
							{/if}
						{:else}
							<tr><td class="dim">{t('fu_runs_none')}</td></tr>
						{/each}
					</tbody>
				</table>
			{/if}
		</div>
	{:else if open === 'senders'}
		<SenderRules sourceId={source.id} {profiles} {onchange} />
	{/if}
</article>

<style>
	.srccard.wide {
		grid-column: 1 / -1;
	}
	.srccard {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}
	.hd {
		display: flex;
		gap: 10px;
		align-items: flex-start;
	}
	.hd .pill {
		margin-left: auto;
	}
	.kind {
		font-size: 11.5px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.facts {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 12px;
		margin: 0;
		font-size: 12px;
	}
	.facts dt {
		color: var(--dim);
	}
	.facts dd {
		margin: 0;
	}
	.facts dd.stale {
		color: var(--stale);
	}
	.err {
		font-size: 11.5px;
		color: var(--out);
		overflow-wrap: anywhere;
	}
	.err2 {
		color: var(--out);
		font-size: 11px;
		white-space: normal;
		overflow-wrap: anywhere;
	}
	.acts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		margin-top: auto;
	}
	.runs {
		border-top: 1px solid var(--line);
		padding-top: 6px;
		overflow-x: auto;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
