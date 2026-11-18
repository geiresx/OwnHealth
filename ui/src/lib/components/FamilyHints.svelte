<script lang="ts">
	// Cross-profile hints on the Family screen (v0.3): a relative chose to let a result of theirs feed a
	// cited hint in this profile (data/family/hints.yaml, under `patterns.hints` in GET /pedigree). A
	// hint names the rule and the relationship only, never the person, the value or its source record.
	import type { FamilyHint } from '#lib/api/family';
	import { cap, tf } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';

	let { hints, relLabel }: { hints: FamilyHint[]; relLabel: (code: string) => string } = $props();
	const lang = $derived(i18n.lang as 'es' | 'en');
</script>

<section class="card" aria-labelledby="fam-hint-h">
	<h3 id="fam-hint-h">{t('fam_hint_t')}<span class="r">{t('fam_hint_r')}</span></h3>
	<ul>
		{#each hints as h, i (i)}
			<li class="banner hint">
				<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="9" /><path d="M12 8v.5M12 11v5" /></svg>
				<div>
					<b>{h.title[lang]}</b>
					<div class="why">{h.why[lang]}</div>
					<div class="meta">
						<span>{tf('fam_hint_from', { rel: cap(relLabel(h.rel)) })}</span>
						<a class="chip" href={h.cite} target="_blank" rel="noopener" title={t('fam_hint_cite')}>{h.source}</a>
						{#if h.basis === 'pack_default'}<span class="chip" title={t('fam_pat_ours_d')}>{t('fam_pat_ours')}</span>{/if}
						<span class="mono">{h.rule}</span>
					</div>
				</div>
			</li>
		{/each}
	</ul>
	<div class="dim note">{t('fam_hint_note')}</div>
</section>

<style>
	h3 .r {
		white-space: normal;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	.hint {
		margin: 0;
		align-items: flex-start;
		padding: 8px 11px;
		border-color: rgba(110, 168, 254, 0.45);
		background: linear-gradient(90deg, rgba(110, 168, 254, 0.12), rgba(110, 168, 254, 0.03));
		color: var(--accent);
	}
	.hint > div {
		min-width: 0;
		color: var(--text);
	}
	.hint b {
		font-size: 12.5px;
	}
	.why {
		font-size: 12px;
		color: var(--muted);
		margin-top: 2px;
	}
	.meta {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: center;
		margin-top: 4px;
		font-size: 11px;
		color: var(--dim);
	}
	.meta a.chip {
		text-decoration: none;
		white-space: normal;
	}
	.mono {
		font-size: 10px;
	}
	.note {
		font-size: 11.5px;
		margin-top: 6px;
	}
</style>
