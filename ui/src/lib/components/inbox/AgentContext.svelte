<script lang="ts">
	// "In the agent's context": the documents and src ids the proposing token read in the hour before
	// a proposal, from the audit log (security-privacy.md 4.2 #4). Shown on every agent proposal, so the
	// reviewer sees what the agent had read when it wrote the value.
	import { tf } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { agentContext } from '#lib/logic/inbox';
	import type { ReviewItem } from '#lib/api/types';

	let { item, docName }: { item: ReviewItem; docName: (id: string) => string } = $props();
	const ctx = $derived(agentContext(item as ReviewItem & Record<string, unknown>));
	let open = $state(false);
</script>

<div class="ac">
	{#if !ctx.documents.length && !ctx.src_ids.length}
		<span class="dim">{t('fu_ctx_none')}</span>
	{:else}
		<button class="lnk" onclick={() => (open = !open)} aria-expanded={open}>{tf('fu_ctx', { d: ctx.documents.length, s: ctx.src_ids.length })}</button>
		{#if open}
			<ul>
				{#each ctx.documents as d (d)}<li><a href="?doc={d}">{docName(d) || d.slice(0, 8)}</a></li>{/each}
				{#each ctx.src_ids as s (s)}<li class="mono dim">{s}</li>{/each}
			</ul>
		{/if}
	{/if}
</div>

<style>
	.ac {
		font-size: 11px;
	}
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		font-size: 11px;
	}
	ul {
		margin: 3px 0 0;
		padding-left: 16px;
	}
	li.mono {
		font-size: 10.5px;
	}
</style>
