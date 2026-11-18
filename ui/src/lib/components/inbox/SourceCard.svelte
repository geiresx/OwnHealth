<script lang="ts">
	// One inbox on the Inbox screen: its status (OK, Out, Partial, Stale, No data, Off), last run, last successful
	// sync, the last error, and its actions. Push sources get a token (shown once by the parent).
	import { listRuns, type InboxSource, type SourceRun } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import { Loader, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import { ago, stamp } from '#lib/components/pages2/fmt';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { afterColon } from '#lib/logic/format';
	import { POLL_KINDS, PROFILE_KINDS, hasAuthserv, sourceStatus, sourceWhere, tokenButtonKey } from '#lib/logic/inbox';
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
		onchange,
		error = null,
		notice = null
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
		/** What the last action on this card did: shown here, next to the buttons that did it. */
		error?: { text: string; raw: string } | null;
		notice?: string | null;
	} = $props();

	const PILL = { ok: 'p-ok', out: 'p-out', partial: 'p-watch', stale: 'p-stale', nodata: 'p-stale', off: 'p-stale' } as const;
	const status = $derived(sourceStatus(source));
	// A paused source has no fresh data: it shows No data (one of the five status words); the switch says paused.
	const pillWord = $derived(status === 'off' ? 'fu_st_nodata' : 'fu_st_' + status);
	const runs = new Loader<SourceRun[]>();
	let open = $state<'' | 'runs' | 'senders'>('');
	let confirming = $state(false);
	// Replacing a live token cuts off whatever uses it, so it waits for a second click.
	let confirmToken = $state(false);
	function toggle(what: 'runs' | 'senders') {
		open = open === what ? '' : what;
		if (open === 'runs') runs.run(async () => (await listRuns(source.id, 10)).items, source.id);
	}
	const c = $derived(source.config as Record<string, string | number | boolean | null | undefined>);
	const where = $derived(sourceWhere(source.config));
	const detail = $derived.by(() => {
		switch (source.kind) {
			case 'email':
				return [c.username, c.host].filter(Boolean).join(' · ');
			case 'folder':
				return `OWNHEALTH_INBOX_DIR/${where.subdir ? where.subdir + '/' : ''}`;
			case 'paperless':
				return [where.url, where.tag ? `tag: ${where.tag}` : null].filter(Boolean).join(' · ');
			case 'nightscout':
				return where.url;
			// The weekly ClinVar check: a local file, a mirror, or NCBI.
			case 'clinvar':
				return String(c.path || c.url || 'ftp.ncbi.nlm.nih.gov');
			// These two never send with a token of their own: say how their files arrive instead.
			case 'gadgetbridge':
				return t('fu_via_upload');
			case 'share':
				return t('fu_via_share');
			default:
				return source.token_prefix ? `${source.token_prefix}…` : t('fu_no_token');
		}
	});
	// Where the data goes. With no profile set, each kind has its own rule; a kind that needs one says so.
	const who = $derived.by(() => {
		if (source.profile_id) return { text: profiles.find((p) => p.id === source.profile_id)?.display_name ?? '—', warn: false };
		if (source.kind === 'email') return { text: t('fu_f_profile_rule'), warn: false };
		if (source.kind === 'folder') return { text: t('fu_f_profile_folder'), warn: false };
		if (source.kind === 'share') return { text: t('fu_to_share'), warn: false };
		if (source.kind === 'clinvar') return { text: t('gen_src_to'), warn: false };
		if (PROFILE_KINDS.includes(source.kind)) return { text: t('fu_to_none'), warn: true };
		return { text: '—', warn: false };
	});
	const lastErr = $derived(source.last_error ? errText(new Error(source.last_error)) : '');
	const runTone = { ok: 'p-ok', partial: 'p-watch', error: 'p-out', running: 'p-info' } as const;
</script>

<!-- An open panel (runs, senders) takes the whole row, so its table never spills into the next card. -->
<article class="card srccard" class:wide={open !== ''} aria-labelledby="src-{source.id}">
	<div class="hd">
		<div style="min-width:0">
			<b id="src-{source.id}">{source.label}</b>
			<div class="dim kind">{t(source.kind === 'clinvar' ? 'gen_cv_set_t' : 'fu_k_' + source.kind)} · <span class:mono={source.kind !== 'gadgetbridge' && source.kind !== 'share'}>{detail}</span></div>
		</div>
		<span class="pill {PILL[status]}"><i aria-hidden="true"></i>{t(pillWord)}</span>
	</div>
	<dl class="facts">
		<dt>{t('fu_to')}</dt>
		<dd style={who.warn ? 'color:var(--watch)' : ''}>{who.text}</dd>
		<dt>{t('fu_last_run')}</dt>
		<dd><span class="num" style="white-space:nowrap">{source.last_run_at ? ago(source.last_run_at) : t('i2_never')}</span></dd>
		<dt>{t('fu_last_ok')}</dt>
		<dd class:stale={status === 'stale'}><span class="num" style="white-space:nowrap">{source.last_success_at ? ago(source.last_success_at) : t('i2_never')}</span></dd>
		{#if source.kind === 'email' && source.quarantined}
			<dt>{t('fu_quarantine')}</dt>
			<dd style="color:var(--watch)">{source.quarantined}</dd>
		{/if}
	</dl>
	{#if status === 'out' && source.last_error}
		<div class="err" role="status">
			⚠ {tp('fu_failed_n', source.consecutive_failures)} {afterColon(lastErr, i18n.lang)}
			{#if lastErr !== source.last_error}<div class="mono dim raw">{source.last_error}</div>{/if}
		</div>
	{:else if status === 'partial'}
		<div class="note" role="status" style="color:var(--watch);overflow-wrap:anywhere">
			⚠ {tp('fu_partial_n', source.last_run_problems ?? 0)}
			{#if source.last_run_problem}<span class="mono">{source.last_run_problem}</span>{/if}
		</div>
	{:else if status === 'stale'}
		<div class="note" style="color:var(--stale)">{t('fu_stale_d')}</div>
	{/if}
	<div class="acts">
		<Switch on={source.enabled} label={tf('fu_enabled', { label: source.label })} words={[t('fu_sw_on'), t('fu_sw_off')]} disabled={busy} onchange={ontoggle} />
		{#if POLL_KINDS.includes(source.kind)}<button class="btn sm" disabled={busy || !source.enabled} onclick={onsync}>{t('fu_sync')}</button>{/if}
		<!-- Every kind gets a token: push kinds send with it, polled kinds give it to a monitor. -->
		<button class="btn sm" disabled={busy} aria-expanded={source.token_prefix ? confirmToken : undefined} onclick={() => (source.token_prefix ? (confirmToken = !confirmToken) : ontoken())}>{t(tokenButtonKey(source.kind, !!source.token_prefix))}</button>
		{#if source.kind === 'email'}<button class="btn sm" onclick={() => toggle('senders')} aria-expanded={open === 'senders'}>{t('fu_senders')}</button>{/if}
		<button class="btn sm" onclick={() => toggle('runs')} aria-expanded={open === 'runs'}>{t('fu_runs')}</button>
		<!-- The ClinVar check comes back by itself while the household has a genome: Settings switches it off. -->
		{#if source.kind === 'clinvar'}
			<a class="btn sm" href="/settings">{t('i2_settings')}</a>
		{:else}
			<button class="btn sm" data-edit={source.id} onclick={onedit}>{t('i2_edit')}</button>
			<button class="btn sm no del" disabled={busy} onclick={() => (confirming = !confirming)} aria-expanded={confirming}>{t('fu_delete')}</button>
		{/if}
	</div>
	{#if confirmToken}
		<ConfirmRow text={t('fu_token_new_d')} confirmLabel={t('fu_token_new_yes')} {busy} onconfirm={() => { confirmToken = false; ontoken(); }} oncancel={() => (confirmToken = false)} />
	{/if}
	{#if confirming}
		<ConfirmRow text={tf('fu_src_delete_d', { label: source.label })} confirmLabel={t('fu_delete_yes')} {busy} onconfirm={ondelete} oncancel={() => (confirming = false)} />
	{/if}
	{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
	{#if notice}<div class="p2ok" role="status" style="font-size:12px">{notice}</div>{/if}
	{#if open === 'runs'}
		<div class="runs">
			{#if runs.error}
				<div class="p2err" role="alert" title={errRaw(runs.error)}>{errText(runs.error)}</div>
			{:else if runs.loading && !runs.data}
				<div class="dim">{t('loading')}</div>
			{:else}
				<table class="t nw" style="width:auto">
					<tbody>
						{#each runs.data ?? [] as r (r.id)}
							<tr>
								<td class="dim"><span class="num" style="white-space:nowrap">{stamp(r.started_at)}</span></td>
								<td class="dim">{t('fu_trig_' + r.trigger)}</td>
								<td><span class="pill {runTone[r.status]}"><i aria-hidden="true"></i>{t('fu_run_' + r.status)}</span></td>
								<td class="num">{tf(source.kind === 'email' ? 'fu_run_counts' : 'fu_run_counts_nq', { seen: r.items_seen, new: r.items_new, dup: r.items_duplicate, q: r.items_quarantined })}</td>
							</tr>
							{@const skipped = Object.keys(r.skipped ?? {})}
							{#if skipped.length}
								<tr><td colspan="4" class="dim">{tp('fu_run_skipped', skipped.length)} <span class="mono">{skipped.join(', ')}</span></td></tr>
							{/if}
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
		<SenderRules sourceId={source.id} {profiles} {onchange} authChecked={hasAuthserv(source.config)} />
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
	.err .raw {
		font-size: 10.5px;
		margin-top: 2px;
	}
	.acts .del {
		margin-left: auto;
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
</style>
