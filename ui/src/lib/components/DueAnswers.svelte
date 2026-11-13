<script lang="ts">
	// The questions some guideline rows need (data/prevention/pack.yaml answer_keys), for one profile:
	// GET/PUT /profiles/{p}/prevention-answers. An answer only decides whether a row applies; the card
	// hides itself when no row of the profile's country needs one and nothing was answered.
	import { getDueAnswers, putDueAnswers } from '#lib/api';
	import { ApiError } from '#lib/api/core';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import type { DueAnswer } from '#lib/logic/due';
	import { errText } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';

	let {
		profile,
		name = '',
		canWrite,
		onchange,
		bare = false
	}: { profile: string; name?: string; canWrite: boolean; onchange?: () => void; bare?: boolean } = $props();

	let items = $state<DueAnswer[] | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let saved = $state<string | null>(null);
	// a load that failed for another reason than the scopes: the card says so and offers a retry
	let loadErr = $state<unknown>(null);
	let reload = $state(0);
	// only the latest request fills the card: a slower one, for this or another profile or language,
	// is dropped
	let gen = 0;

	$effect(() => {
		const slug = profile;
		const lang = i18n.lang;
		void reload;
		const g = ++gen;
		items = null;
		error = null;
		loadErr = null;
		getDueAnswers(slug, lang).then(
			(r) => {
				if (g === gen) items = r.items;
			},
			(e) => {
				if (g !== gen) return;
				items = [];
				// a caller without read:records gets no card at all
				if (!(e instanceof ApiError && e.status === 403)) loadErr = e;
			}
		);
	});

	const VALUES = [
		['yes', 'due_ans_yes'],
		['no', 'due_ans_no'],
		[null, 'due_ans_unset']
	] as const;

	async function set(key: string, value: 'yes' | 'no' | null) {
		if (busy) return;
		const g = ++gen;
		const slug = profile;
		busy = true;
		error = null;
		saved = null;
		try {
			const r = await putDueAnswers(slug, { [key]: value }, i18n.lang);
			if (g === gen) {
				items = r.items;
				saved = key;
			} else if (slug === profile) {
				// a load started while the answer was saving may predate it: load again
				reload++;
			}
			onchange?.();
		} catch (e) {
			error = errText(e);
		} finally {
			busy = false;
		}
	}
</script>

{#if items?.length}
	<section class:card={!bare} id="due-answers-{profile}" aria-labelledby="due-ans-{profile}">
		<h3 id="due-ans-{profile}" class:sub={bare}>{t('due_ans_title')}{#if name}<span class="r">{name}</span>{/if}</h3>
		{#each items as a (a.key)}
			<div class="q">
				<div class="qt">{a.question ?? a.key}</div>
				<div class="seg" role="group" aria-label={a.question ?? a.key}>
					{#each VALUES as [v, label] (label)}
						<button type="button" class:on={a.value === v} aria-pressed={a.value === v} disabled={!canWrite || busy} onclick={() => set(a.key, v)}>{t(label)}</button>
					{/each}
				</div>
				{#if saved === a.key}<span class="p2ok" role="status">{t('due_ans_saved')}</span>{/if}
			</div>
		{/each}
		{#if error}<div class="p2err" role="alert">{error}</div>{/if}
		<div class="note">{t('due_ans_note')}</div>
	</section>
{:else if loadErr}
	<section class:card={!bare} aria-labelledby="due-ans-{profile}">
		<h3 id="due-ans-{profile}" class:sub={bare}>{t('due_ans_title')}{#if name}<span class="r">{name}</span>{/if}</h3>
		<NotReady error={loadErr} onretry={() => reload++} />
	</section>
{/if}

<style>
	.q {
		display: flex;
		gap: 10px;
		align-items: center;
		flex-wrap: wrap;
		padding: 7px 0;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	.qt {
		flex: 1 1 260px;
		min-width: 0;
		color: var(--text);
	}
	.sub {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--dim);
		margin: 10px 0 2px;
		font-weight: 400;
	}
	.p2ok {
		font-size: 11.5px;
	}
	.note {
		margin-top: 6px;
	}
</style>
