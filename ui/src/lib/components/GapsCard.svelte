<script lang="ts">
	// The timeline's Gaps card (mockup screen #timeline): visits with no report on file and long stretches
	// with no records, from GET /profiles/{p}/gaps (ownhealth.gaps/v1). Each gap names the src that raised it
	// and cites its rule; a visit gap in a country with a letter template links the printable access-request
	// letter in Spanish and in English. The same gaps are items of the Due list, where they can be snoozed,
	// marked done or dismissed: a gap decided there is listed dimmed under the open ones, with what was
	// decided, and only open gaps count. The server's own day is the day of the list, as for the letter,
	// so a gap listed here always has its letter. `stamp` changes when the timeline reloads, so a new
	// record refreshes the card.
	import { gapLetterUrl, getGaps } from '#lib/api';
	import type { GapItem, Gaps } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';

	let { profile, stamp }: { profile: string; stamp?: unknown } = $props();
	const gaps = new Loader<Gaps>();

	function load() {
		void stamp;
		const slug = profile;
		const lang = i18n.lang;
		gaps.run(() => getGaps(slug, lang), `${slug}/${lang}`);
	}
	$effect(load);

	// the day each src stands for: the visit's, or a hole's two edges when both are records (an edge that is a
	// document has no src id, so a lone src is left unlabelled rather than guessed)
	const srcDay = (g: GapItem, k: number) => (g.date ? g.date : g.src.length === 2 ? ((k ? g.to : g.from) ?? '') : '');

	// what the person decided on the Due list
	const decided = (g: GapItem) =>
		g.decision?.kind === 'snooze' ? tf('gap_snoozed', { d: g.decision.until ?? '' }) : g.decision?.kind === 'done' ? t('gap_done') : t('gap_dismissed');

	const LETTER_LANGS: [string, string][] = [
		['es', 'Español'],
		['en', 'English']
	];
</script>

<div class="card">
	<h3>{t('h1_gaps')}{#if gaps.data?.items.length}<span class="r num">{gaps.data.items.length}</span>{/if}</h3>
	{#if gaps.error}
		<NotReady error={gaps.error} onretry={load} />
	{:else if !gaps.data}
		<div class="dim" style="font-size:12px">{t('loading')}</div>
	{:else}
		{@const d = gaps.data}
		{#each d.items as g (g.key)}
			<div class="due gap">
				<div class="ic" aria-hidden="true">!</div>
				<div class="w">
					<div>{g.title}</div>
					<div class="meta">
						<span>{g.why}</span>
						{#if g.rule === 'visit_without_report'}
							{#if g.letter}
								<span class="letter">
									{t('gap_letter')}:
									{#each LETTER_LANGS as [lang, name] (lang)}
										<a class="chip" href={gapLetterUrl(profile, g.key, lang)} target="_blank" rel="noopener" title={t('gap_letter_t')} lang={lang}>{name}</a>
									{/each}
								</span>
							{:else}
								<span>{t('gap_no_letter')}</span>
							{/if}
						{/if}
						{#each g.src as s, k (s)}<SourceChip label={srcDay(g, k)} name={g.title} src={s} onopen={openSource} />{/each}
						<a class="chip" href={g.cite} target="_blank" rel="noopener" title={t('gap_cite_t')}>{g.source}</a>
					</div>
				</div>
			</div>
		{:else}
			{#if !d.hidden?.length}<div class="dim" style="font-size:12px">{d.withheld ? '' : t('gap_none')}</div>{/if}
		{/each}
		{#each d.hidden ?? [] as g (g.key)}
			<div class="due gap decided">
				<div class="ic" aria-hidden="true">✓</div>
				<div class="w">
					<div>{g.title}</div>
					<div class="meta"><span>{decided(g)}</span></div>
				</div>
			</div>
		{/each}
		{#if d.withheld}<div class="note" style="margin-top:6px">{tf('gap_withheld', { n: d.withheld, scopes: d.withheld_scopes.join(', ') })}</div>{/if}
		{#if d.items.length || d.hidden?.length}<div class="dim" style="font-size:11.5px;margin-top:6px"><a href="/due">{t('gap_due_link')}</a></div>{/if}
	{/if}
</div>

<style>
	.gap {
		align-items: flex-start;
	}
	.gap .ic {
		background: var(--watch-bg);
		color: var(--watch);
		font-weight: 700;
		font-size: 13px;
	}
	.decided {
		opacity: 0.65;
	}
	.decided .ic {
		background: var(--ok-bg);
		color: var(--ok);
	}
	.meta {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		margin-top: 2px;
	}
	.letter {
		display: inline-flex;
		flex-wrap: wrap;
		gap: 4px;
		align-items: center;
		color: var(--text);
	}
</style>
