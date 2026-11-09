<script lang="ts">
	// One extracted row waiting in the review queue: the mockup's `.xr` grid, with accept, edit,
	// reject (with a reason), "add alias" for unknown names, and quarantine for untrusted text.
	import type { Check, ReviewItem } from '#lib/api/types';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { tf } from '#lib/components/pages1/util.svelte';
	import AgentContext from '#lib/components/inbox/AgentContext.svelte';
	import { rowGrounding } from '#lib/logic/inbox';

	type Obs = Record<string, string | number | null | undefined>;
	let {
		item,
		selected = false,
		checked = false,
		oncheck,
		busy = false,
		threshold = 0.95,
		onselect,
		onaccept,
		onreject,
		ondiscard,
		onalias,
		docName = () => ''
	}: {
		item: ReviewItem;
		selected?: boolean;
		checked?: boolean;
		oncheck: (on: boolean) => void;
		busy?: boolean;
		threshold?: number;
		onselect: () => void;
		onaccept: (edits?: Record<string, unknown>) => void;
		onreject: (reason: string) => void;
		ondiscard: () => void;
		onalias: (body: { alias_group: string; loinc: string | null }) => void;
		docName?: (id: string) => string;
	} = $props();

	const pl = $derived(item.payload as { observation?: Obs; checks?: Check[]; cells?: Record<string, { text?: string }> });
	const obs = $derived(pl.observation ?? {});
	const failed = $derived((pl.checks ?? []).filter((c) => !c.ok && !GROUNDING_REASONS.includes(c.reason ?? '')));
	const quarantined = $derived(item.reasons.includes('possible_injection'));
	const unknownName = $derived(item.reasons.some((r) => r === 'loinc_unknown' || r === 'unmapped'));
	const conf = $derived(item.row_conf ?? 0);
	const lowConf = $derived(item.row_conf != null && item.row_conf < threshold);
	// v0.2: a value not found in the page's word layer is red (confidence 0); a page with no word layer
	// (a scan while OCR is off) is amber, and the person compares each digit with the page image.
	const grounding = $derived(rowGrounding(item));
	const wordLayer = $derived((item.payload as { word_layer?: string }).word_layer ?? null);
	const agent = $derived(item.kind === 'agent_proposal' || item.proposed_by_token != null);
	const tone = $derived(
		quarantined || grounding === 'ungrounded' ? 'bad' : item.reasons.length || lowConf || grounding === 'no_word_layer' ? 'low' : agent ? 'agent' : ''
	);
	// The grounding reasons have their own line below; keep them out of the generic check list.
	const GROUNDING_REASONS = ['not_grounded', 'ungrounded_no_word_layer'];
	const printed = $derived([obs.printed_value, obs.printed_unit].filter(Boolean).join(' '));
	const canonical = $derived(obs.value_canonical != null ? `${obs.value_canonical} ${obs.unit_canonical ?? ''}`.trim() : '');
	const rowText = $derived(Object.values(pl.cells ?? {}).map((c) => c.text ?? '').join(' · '));

	let mode = $state<'' | 'edit' | 'reject' | 'alias' | 'view'>('');
	let reason = $state('');
	let ed = $state({ value_num: '', unit_ucum: '', loinc: '', alias_group: '' });
	let alias = $state({ alias_group: '', loinc: '' });

	function open(m: typeof mode) {
		mode = mode === m ? '' : m;
		if (m === 'edit') ed = { value_num: String(obs.value_num ?? ''), unit_ucum: String(obs.unit_ucum ?? ''), loinc: String(obs.loinc ?? ''), alias_group: String(obs.alias_group ?? '') };
		if (m === 'alias') alias = { alias_group: String(obs.alias_group ?? ''), loinc: String(obs.loinc ?? '') };
		reason = '';
	}
	function saveEdit(e: SubmitEvent) {
		e.preventDefault();
		const edits: Record<string, unknown> = {};
		for (const [k, v] of Object.entries(ed)) if (v !== String(obs[k] ?? '')) edits[k] = v === '' ? null : v;
		onaccept(Object.keys(edits).length ? edits : undefined);
		mode = '';
	}
	// The translated reason is the line; the backend's detail (English, with specifics) is the tooltip,
	// and in English a dim second part. A detail that is only a code never stands in for the reason.
	const label = (c: Check) => {
		const k = 'i2_reason_' + c.reason;
		return t(k) !== k ? t(k) : null;
	};
	const isCode = (d: string) => /^[\w:.-]+$/.test(d.trim()) && !/\s/.test(d.trim());
	const msg = (c: Check) => label(c) ?? (c.detail && !isCode(c.detail) ? c.detail : t('i2_reason_other'));
	const extra = (c: Check) => (i18n.lang === 'en' && c.detail && label(c) && !isCode(c.detail) ? c.detail : '');
	const id = $derived(`rr-${item.id.slice(0, 8)}`);
</script>

<div class="xr {tone}" class:sel={selected} role="group" aria-labelledby="{id}-n">
	<input type="checkbox" {checked} onchange={(e) => oncheck(e.currentTarget.checked)} aria-label="{t('i2_select')} {obs.printed_name}" disabled={quarantined} />
	<div>
		<button class="nm" id="{id}-n" onclick={onselect} aria-pressed={selected} title={t('i2_show_on_page')}>{obs.printed_name}</button>
		<div style="font-size:10.5px" class="dim">{t('i2_range')} {obs.printed_ref_range || '—'}</div>
	</div>
	<div class="mono" style="font-size:11.5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title="{obs.loinc ?? ''} {obs.alias_group ?? ''}">
		{#if quarantined}—{:else}{obs.loinc ?? t('i2_no_loinc')} <span class="dim">{obs.alias_group}</span>{/if}
	</div>
	<div class="num" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">
		{#if quarantined}{t('i2_free_text')}{:else}{printed}{#if canonical && canonical !== printed}<span class="dim"> → {canonical}</span>{/if}{/if}
	</div>
	<div class="conf">
		{#if quarantined}
			<span class="pill p-out"><i aria-hidden="true"></i>{t('i2_quarantined')}</span>
		{:else}
			<div class="bar" aria-hidden="true"><i style="width:{conf * 100}%;background:{conf > 0.9 ? 'var(--ok)' : 'var(--watch)'}"></i></div>
			<span class="num" aria-label="{t('i2_confidence')} {conf.toFixed(2)}">{conf.toFixed(2)}</span>
		{/if}
	</div>
	<div class="acts" style="justify-content:flex-end">
		{#if quarantined}
			<button class="btn sm" onclick={() => open('view')} aria-expanded={mode === 'view'}>{t('i2_view')}</button>
			<button class="btn sm no" onclick={ondiscard} disabled={busy}>{t('i2_discard')}</button>
		{:else}
			<button class="btn sm ok" onclick={() => onaccept()} disabled={busy} aria-label="{t('i2_accept')} {obs.printed_name}" title={t('i2_accept')}>✓</button>
			<button class="btn sm" onclick={() => open('edit')} aria-expanded={mode === 'edit'}>{t('i2_edit')}</button>
			<button class="btn sm no" onclick={() => open('reject')} aria-expanded={mode === 'reject'} aria-label="{t('i2_reject')} {obs.printed_name}" title={t('i2_reject')}>✕</button>
		{/if}
	</div>
	{#each failed as c, i (i)}
		<div class="why" style="color:{quarantined ? 'var(--out)' : 'var(--watch)'}" title={c.detail ?? undefined}>{quarantined ? '⚠' : '●'} {msg(c)}{#if extra(c)}<span class="dim"> · {extra(c)}</span>{/if}</div>
	{/each}
	{#if grounding === 'ungrounded'}
		<div class="why" style="color:var(--out)" role="note">✕ {t('fu_ungrounded')}</div>
	{:else if grounding === 'no_word_layer'}
		<div class="why" style="color:var(--watch)" role="note">● {t('fu_no_word_layer')}</div>
	{/if}
	{#if agent}
		<div class="why" style="color:var(--accent)">◆ {t('i2_row_agent')}{#if grounding === 'grounded'} · {tf('fu_grounded', { p: Number((item.payload as { page_no?: number }).page_no ?? 1) })}{/if}</div>
		<div class="why"><AgentContext {item} {docName} /></div>
	{:else if wordLayer === 'ocr' || wordLayer === 'paperless_ocr'}
		<div class="why dim">{t(wordLayer === 'ocr' ? 'fu_wl_ocr' : 'fu_wl_paperless')}{#if grounding === 'grounded'} · {tf('fu_grounded', { p: Number((item.payload as { page_no?: number }).page_no ?? 1) })}{/if}</div>
	{/if}
	{#if !failed.length && lowConf && grounding !== 'ungrounded'}
		<div class="why" style="color:var(--watch)">● {tf('i2_low_conf', { c: conf.toFixed(2), t: threshold.toFixed(2) })}</div>
	{/if}
	{#if unknownName && !quarantined && mode !== 'alias'}
		<div class="why"><button class="lnk" onclick={() => open('alias')}>{t('i2_add_alias')}</button> <span class="dim">{t('i2_add_alias_d')}</span></div>
	{/if}
	{#if mode === 'view'}
		<div class="why"><pre class="p2untrusted">{rowText}</pre><span class="dim" style="font-size:10.5px">trust: untrusted_text</span></div>
	{:else if mode === 'reject'}
		<form class="why p2row" onsubmit={(e) => { e.preventDefault(); onreject(reason.trim()); mode = ''; }}>
			<input class="p2in" style="flex:1" bind:value={reason} required placeholder={t('i2_reject_reason')} aria-label={t('i2_reject_reason')} />
			<button class="btn sm no" type="submit" disabled={busy}>{t('i2_reject')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{:else if mode === 'edit'}
		<form class="why p2row" onsubmit={saveEdit}>
			<input class="p2in num" style="width:80px" bind:value={ed.value_num} aria-label={t('i2_value')} placeholder={t('i2_value')} />
			<input class="p2in" style="width:90px" bind:value={ed.unit_ucum} aria-label={t('i2_unit')} placeholder={t('i2_unit')} />
			<input class="p2in mono" style="width:90px" bind:value={ed.loinc} aria-label="LOINC" placeholder="LOINC" />
			<input class="p2in mono" style="width:120px" bind:value={ed.alias_group} aria-label={t('i2_group')} placeholder={t('i2_group')} required />
			<button class="btn sm ok" type="submit" disabled={busy}>{t('i2_save_accept')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{:else if mode === 'alias'}
		<form class="why p2row" onsubmit={(e) => { e.preventDefault(); onalias({ alias_group: alias.alias_group.trim(), loinc: alias.loinc.trim() || null }); mode = ''; }}>
			<span class="dim">“{obs.printed_name}” →</span>
			<input class="p2in mono" style="width:130px" bind:value={alias.alias_group} required aria-label={t('i2_group')} placeholder={t('i2_group')} />
			<input class="p2in mono" style="width:90px" bind:value={alias.loinc} aria-label="LOINC" placeholder="LOINC" />
			<button class="btn sm" type="submit" disabled={busy}>{t('i2_add_alias')}</button>
			<button class="btn sm" type="button" onclick={() => (mode = '')}>{t('i2_cancel')}</button>
		</form>
	{/if}
</div>

<style>
	.nm {
		font-weight: 560;
		background: none;
		border: 0;
		padding: 0;
		text-align: left;
		color: var(--text);
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.nm:hover {
		color: var(--accent);
	}
	.why {
		grid-column: 2/-1;
		font-size: 11px;
	}
	.sel {
		box-shadow: inset 3px 0 0 var(--accent);
	}
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		font-size: 11px;
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
