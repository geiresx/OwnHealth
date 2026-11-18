<script lang="ts">
	// Settings > profile > Calendar feed: a private address a calendar app polls for this profile's due
	// items (GET /api/ics/<token>.ics). The address is shown once, right after it is made; the list shows
	// its prefix, whether events name the item, when a calendar last fetched it, and Revoke. Making one
	// and naming the items need step-up. A caller without read:records gets no section at all.
	import { ApiError } from '#lib/api/core';
	import { createCalendarFeed, listCalendarFeeds, revokeCalendarFeed, setCalendarFeedDetail, type CalendarFeed, type FeedDetail } from '#lib/api/calendar';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { errRaw, errText } from '#lib/components/pages1/util.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import { ago, day } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { copyText } from '#lib/logic/inbox';

	// `locks` is the profile's lock list: a change to it can give or take away read:records, so the
	// list loads again and the section shows or hides to match.
	let { profile, name, locks = [] }: { profile: string; name: string; locks?: readonly string[] } = $props();

	let feeds = $state<CalendarFeed[] | null>(null);
	let hidden = $state(false);
	let loadErr = $state<unknown>(null);
	let reload = $state(0);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	let confirming = $state<string | null>(null);
	// The address of the feed just made: in this page's memory only, gone on reload.
	let fresh = $state<{ id: string; url: string } | null>(null);
	let copied = $state<boolean | null>(null);
	let urlEl: HTMLElement | undefined = $state();
	let gen = 0;

	$effect(() => {
		const slug = profile;
		void reload;
		void locks.join();
		const g = ++gen;
		feeds = null;
		loadErr = null;
		listCalendarFeeds(slug).then(
			(r) => {
				if (g !== gen) return;
				feeds = r.items;
				hidden = false;
			},
			(e) => {
				if (g !== gen) return;
				// a 403 hides the section; any other failure shows it with its retry
				hidden = e instanceof ApiError && e.status === 403;
				if (!hidden) loadErr = e;
			}
		);
	});

	async function act(fn: () => Promise<unknown>) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
			confirming = null;
			reload++;
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	const make = () =>
		act(async () => {
			const r = await createCalendarFeed(profile, 'generic');
			fresh = { id: r.feed.id, url: location.origin + r.path };
			copied = null;
		});
	const setDetail = (f: CalendarFeed, named: boolean) => act(() => setCalendarFeedDetail(profile, f.id, (named ? 'named' : 'generic') as FeedDetail));
	const revoke = (f: CalendarFeed) =>
		act(async () => {
			await revokeCalendarFeed(profile, f.id);
			if (fresh?.id === f.id) fresh = null;
		});
	async function copy() {
		if (fresh) copied = await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, fresh.url, urlEl);
	}
</script>

{#if !hidden}
	<section class="ics" aria-labelledby="ics-{profile}">
		<h4 id="ics-{profile}">{t('due_ics_title')}</h4>
		<div class="note">{t('due_ics_d')}</div>
		{#if loadErr}
			<NotReady error={loadErr} onretry={() => reload++} />
		{:else if !feeds}
			<div class="empty">{t('loading')}</div>
		{:else}
			{#each feeds as f (f.id)}
				<div class="set feed">
					<div style="flex:1;min-width:0">
						<div class="mono" style="font-size:11.5px">{f.prefix}…</div>
						<div class="dim" style="font-size:11px">{t('due_ics_last')}: <span class="num">{ago(f.last_used_at)}</span> · {t('i2_added')} <span class="num">{day(f.created_at)}</span></div>
					</div>
					<Switch on={f.detail === 'named'} label="{t('due_ics_names')} · {name} · {f.prefix}" words={[t('due_ics_named'), t('due_ics_generic')]} disabled={busy} onchange={(on) => setDetail(f, on)} />
					<button class="btn sm no" disabled={busy} aria-expanded={confirming === f.id} aria-label="{t('i2_revoke')} {f.prefix}" onclick={() => (confirming = f.id)}>{t('i2_revoke')}</button>
				</div>
				{#if fresh?.id === f.id}
					<div class="fresh" aria-live="polite">
						<div class="p2row">
							<div class="p2secret url" bind:this={urlEl}>{fresh.url}</div>
							<button class="btn sm" onclick={copy}>{copied ? t('i2_copied') : t('i2_copy')}</button>
						</div>
						{#if copied === false}<div class="note" role="alert" style="color:var(--watch)">{t('due_ics_copy_hand')}</div>{/if}
						<div class="note" style="color:var(--watch)">{t('due_ics_once')}</div>
					</div>
				{/if}
				{#if confirming === f.id}
					<ConfirmRow text={t('due_ics_revoke_d')} confirmLabel={t('i2_revoke')} {busy} onconfirm={() => revoke(f)} oncancel={() => (confirming = null)} />
				{/if}
			{:else}
				<div class="dim" style="font-size:12px;padding:4px 0">{t('due_ics_none')}</div>
			{/each}
			<div class="p2row" style="margin-top:6px">
				<button class="btn sm" disabled={busy} onclick={make} aria-label="{t('due_ics_new')} · {name}">{t('due_ics_new')}</button>
			</div>
			{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
			<div class="note">{t('due_ics_names_d')}</div>
			<div class="note">{t('due_ics_google')}</div>
		{/if}
	</section>
{/if}

<style>
	h4 {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--dim);
		margin: 10px 0 2px;
		font-weight: 400;
	}
	.feed {
		flex-wrap: wrap;
	}
	.fresh {
		display: grid;
		gap: 4px;
		margin: 2px 0 6px;
	}
	.url {
		flex: 1;
		min-width: 0;
		overflow-wrap: anywhere;
		font-size: 11px;
	}
	.note {
		margin-top: 4px;
	}
</style>
