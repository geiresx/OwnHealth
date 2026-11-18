<script lang="ts">
	// The two views of the Due screen: its items, and the next-draw planner. Plain links, so each view
	// has its own address and the browser's back button moves between them.
	import { page } from '$app/state';
	import { t } from '#lib/i18n/index.svelte';

	const TABS = [
		{ href: '/due', key: 'due_draw_tab_list' },
		{ href: '/due/next-draw', key: 'due_draw_tab' }
	];
	const here = $derived(page.url.pathname.replace(/\/$/, '') || '/');
</script>

<nav class="seg tabs" aria-label={t('due_draw_tabs')}>
	{#each TABS as tab (tab.href)}
		<a href={tab.href} class:on={here === tab.href} aria-current={here === tab.href ? 'page' : undefined}>{t(tab.key)}</a>
	{/each}
</nav>

<style>
	.tabs {
		margin-bottom: 12px;
	}
	.tabs a {
		padding: 5px 12px;
		font-size: 12.5px;
		color: var(--muted);
		text-decoration: none;
	}
	.tabs a.on {
		background: var(--accent-bg);
		color: var(--text);
	}
	.tabs a:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: -2px;
	}
</style>
