<script lang="ts">
	// The pedigree card of the Family screen (v0.3 family history): our own SVG drawing of this profile's
	// relatives (logic in #lib/logic/pedigree), a name toggle that is on by default so a screenshot or a
	// printout shows no names, and the add/edit drawer. Data: GET /profiles/{p}/pedigree and /family.
	import {
		addFamilyMember,
		getPedigree,
		listFamily,
		removeFamilyMember,
		updateFamilyMember,
		type FamilyCatalog,
		type FamilyCondition,
		type FamilyMember,
		type MemberIn,
		type Pedigree,
		type Sex
	} from '#lib/api/family';
	import type { ProfileSummary } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import FamilyPatterns from '#lib/components/FamilyPatterns.svelte';
	import FamilyHints from '#lib/components/FamilyHints.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, cap, errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { addEach, fromHousehold, layout, type Box } from '#lib/logic/pedigree';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { tick, untrack } from 'svelte';

	let { profile, profiles }: { profile: ProfileSummary; profiles: ProfileSummary[] } = $props();

	const ped = new Loader<Pedigree>();
	const fam = new Loader<{ items: FamilyMember[]; catalog: FamilyCatalog }>();
	function load() {
		const slug = profile.slug;
		ped.run(() => getPedigree(slug), slug);
		fam.run(() => listFamily(slug), slug);
	}
	$effect(load);

	let hideNames = $state(true);
	// the pattern whose relatives the drawing shades; it clears when the pattern is gone (a new profile, an edit)
	let pattern = $state<string | null>(null);
	const patterns = $derived(ped.data && ped.data.profile.slug === profile.slug ? ped.data.patterns : null);
	const shade = $derived(new Set(patterns?.items.find((p) => p.rule === pattern)?.members ?? []));
	$effect(() => {
		if (pattern && patterns && !patterns.items.some((p) => p.rule === pattern)) pattern = null;
	});
	const lang = $derived(i18n.lang as 'es' | 'en');
	const words = $derived({
		proband: t('fam_proband'),
		masked: t('fam_masked'),
		died: (a: number | null) => (a == null ? '†' : `† ${a}`),
		more: (n: number) => tf('fam_more', { n })
	});
	const drawing = $derived(ped.data && ped.data.profile.slug === profile.slug ? layout(ped.data, lang, hideNames, words) : null);
	const canWrite = $derived(['owner', 'guardian', 'caregiver'].includes(profile.role));
	// The server links only a profile the caller can read (read:records there).
	const readable = $derived(profiles.filter((p) => p.scopes.includes('read:records')));
	const linkable = $derived(readable.filter((p) => p.slug !== profile.slug));
	const gen = (code: string) => fam.data?.catalog.relationships.find((r) => r.code === code)?.generation ?? 0;
	// oldest generation first, as the drawing reads
	const items = $derived([...(fam.data?.items ?? [])].sort((a, b) => gen(a.relationship) - gen(b.relationship)));
	const catalog = $derived(fam.data?.catalog ?? null);
	const relLabel = (code: string) => {
		const r = catalog?.relationships.find((x) => x.code === code);
		return r ? r[lang] : code;
	};
	const kindLabel = (kind: string) => {
		const k = catalog?.conditions.find((x) => x.kind === kind);
		return k ? k[lang === 'es' ? 'short_es' : 'short_en'] : kind;
	};
	// The founder's household relationships are relative to the founder, so only the founder fills from them.
	const fill = $derived(
		canWrite && profile.relationship === 'self'
			? fromHousehold(readable, profile.slug, new Set(items.flatMap((i) => (i.linked_profile ? [i.linked_profile.slug] : []))))
			: []
	);

	// ---- the drawer ----
	interface Draft {
		relationship: string;
		display_name: string;
		sex_at_birth: Sex | '';
		born_year: number | null;
		deceased: boolean;
		death_age: number | null;
		linked_profile: string;
		conditions: FamilyCondition[];
		sensitivity: string[];
	}
	let editing = $state<FamilyMember | null>(null);
	let draft = $state<Draft | null>(null);
	let busy = $state(false);
	let error = $state<{ text: string; raw: string } | null>(null);
	let confirming = $state(false);
	const blank = (): Draft => ({ relationship: 'FTH', display_name: '', sex_at_birth: '', born_year: null, deceased: false, death_age: null, linked_profile: '', conditions: [], sensitivity: [] });
	function openNew() {
		editing = null;
		draft = blank();
		error = null;
		confirming = false;
	}
	// The drawing's nodes are SVG, which focusOnOpen cannot give focus back to: we do it on close.
	let opener = $state<string | null>(null);
	function openEdit(id: string | null, fromNode = false) {
		const m = items.find((i) => i.id === id);
		if (!m || !canWrite) return;
		opener = fromNode ? m.id : null;
		editing = m;
		error = null;
		confirming = false;
		// linked to a profile the caller cannot read: it can only be removed
		if (m.masked) {
			draft = null;
			return;
		}
		draft = {
			relationship: m.relationship,
			display_name: m.display_name ?? '',
			sex_at_birth: m.sex_at_birth ?? '',
			born_year: m.born_year,
			deceased: m.deceased,
			death_age: m.death_age,
			linked_profile: m.linked_profile?.slug ?? '',
			conditions: m.conditions.map((c) => ({ ...c })),
			sensitivity: [...m.sensitivity]
		};
		error = null;
		confirming = false;
	}
	function close() {
		draft = null;
		editing = null;
		error = null;
		confirming = false;
		const id = opener;
		opener = null;
		if (id) tick().then(() => svgwrap?.querySelector<SVGGElement>(`g[data-id="${id}"]`)?.focus());
	}
	async function act(fn: () => Promise<unknown>) {
		busy = true;
		error = null;
		try {
			await fn();
			close();
			load();
		} catch (e) {
			error = { text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	function save(e: SubmitEvent) {
		e.preventDefault();
		const d = draft;
		if (!d) return;
		const body: MemberIn = {
			relationship: d.relationship,
			display_name: d.display_name.trim() || null,
			sex_at_birth: d.sex_at_birth || null,
			born_year: d.born_year ?? null,
			deceased: d.deceased,
			death_age: d.deceased ? (d.death_age ?? null) : null,
			conditions: d.conditions.map((c) => ({ ...c, text: c.text?.trim() || null, onset_age: c.onset_age ?? null })),
			sensitivity: d.sensitivity,
			linked_profile: d.linked_profile || null
		};
		const slug = profile.slug;
		const m = editing;
		act(() => (m ? updateFamilyMember(slug, m.id, body) : addFamilyMember(slug, body)));
	}
	function remove() {
		const m = editing;
		if (m) act(() => removeFamilyMember(profile.slug, m.id));
	}
	function fillAll() {
		const slug = profile.slug;
		const rows = fill;
		act(async () => {
			const failed = await addEach(rows, (r) => addFamilyMember(slug, r));
			if (failed !== undefined) {
				// the other rows are saved: reload so a retry sends only the one that failed
				load();
				throw failed;
			}
		});
	}
	function setKind(c: FamilyCondition, kind: string) {
		c.kind = kind;
		// a new kind is a new code: the server writes the catalog code for it
		c.code = null;
		c.code_system = null;
		if (kind !== 'other') c.text = null;
	}
	const condName = (kind: string) => {
		const k = catalog?.conditions.find((x) => x.kind === kind);
		return k ? k[lang] : kind;
	};

	// ---- drawing helpers ----
	function keyOpen(e: KeyboardEvent, b: Box) {
		if (e.key === 'Enter' || e.key === ' ') {
			e.preventDefault();
			openEdit(b.id, true);
		}
	}
	const editable = (b: Box) => canWrite && !b.proband;
	// On a narrow screen the drawing scrolls sideways: start with the proband in the middle, and say so.
	let svgwrap = $state<HTMLDivElement | null>(null);
	let wide = $state(false);
	function centre() {
		const w = svgwrap;
		const me = drawing?.nodes.find((n) => n.proband);
		if (!w || !drawing || !me) return;
		wide = w.scrollWidth > w.clientWidth + 1;
		if (wide) w.scrollLeft = (me.x * w.scrollWidth) / drawing.width - w.clientWidth / 2;
	}
	$effect(() => {
		if (drawing && svgwrap) tick().then(centre);
	});
	// a pattern just picked: bring its first relative into view, in the page and in a tree that scrolls sideways
	$effect(() => {
		const picked = pattern;
		if (!picked) return;
		untrack(() => {
			const first = patterns?.items.find((p) => p.rule === picked)?.members[0];
			if (first) tick().then(() => svgwrap?.querySelector(`g[data-id="${first}"]`)?.scrollIntoView({ block: 'nearest', inline: 'center' }));
		});
	});
	const listLine = (m: FamilyMember) => {
		const head = cap(relLabel(m.relationship)) + (!hideNames && m.name ? ` · ${m.name}` : '');
		if (m.masked) return `${head} · ${t('fam_masked')}`;
		const bits: string[] = [];
		if (m.deceased) bits.push(m.death_age == null ? '†' : `† ${m.death_age}`);
		else if (m.born_year != null) bits.push(String(m.born_year));
		for (const c of m.conditions) bits.push((c.text ?? condName(c.kind)) + (c.onset_age == null ? '' : ` (${c.onset_approx ? '~' : ''}${c.onset_age})`));
		return bits.length ? `${head} · ${bits.join(', ')}` : head;
	};
</script>

{#snippet symbol(b: Box)}
	<title>{b.title}</title>
	{#if b.sex === 'male'}
		<rect x={b.x - 18} y={b.y - 18} width="36" height="36" class="sym" />
	{:else if b.sex === 'female'}
		<circle cx={b.x} cy={b.y} r="19" class="sym" />
	{:else}
		<path d="M{b.x} {b.y - 21} L{b.x + 21} {b.y} L{b.x} {b.y + 21} L{b.x - 21} {b.y} Z" class="sym" />
	{/if}
	{#if b.deceased}<line x1={b.x - 24} y1={b.y + 24} x2={b.x + 24} y2={b.y - 24} class="dead" />{/if}
	{#if b.proband}<path d="M{b.x - 46} {b.y + 34} L{b.x - 25} {b.y + 15} M{b.x - 25} {b.y + 15} l-9 1 M{b.x - 25} {b.y + 15} l-1 9" class="arrow" />{/if}
	{#each b.lines as line, i (i)}<text x={b.x} y={b.y + 36 + i * 14} class={i === 0 ? 'l1' : 'l2'}>{line}</text>{/each}
{/snippet}

<svelte:window onresize={centre} />

<div class="card ped">
	<h3>{t('h1_pedigree')}<span class="r">{t('fam_r')}</span></h3>
	<div class="p2row tools">
		<label class="hide"><input type="checkbox" bind:checked={hideNames} /> {t('fam_hide')}</label>
		<span class="dim hint">{t('fam_hide_d')}</span>
		{#if canWrite && catalog}
			<span class="grow"></span>
			{#if fill.length}<button class="btn" onclick={fillAll} disabled={busy} title={t('fam_fill_d')}>{t('fam_fill')}</button>{/if}
			<button class="btn" onclick={() => (draft && !editing ? close() : openNew())} aria-expanded={!!draft && !editing}>{t('fam_add')}</button>
		{/if}
	</div>

	{#if ped.error && !ped.data}
		<NotReady error={ped.error} onretry={load} />
	{:else if !drawing}
		<div class="empty">{t('loading')}</div>
	{:else}
		<div class="svgwrap" bind:this={svgwrap}>
			<svg viewBox="0 0 {drawing.width} {drawing.height}" width={drawing.width} style="min-width:{Math.round(drawing.width * 0.8)}px" role="group" aria-label={t('h1_pedigree')} data-testid="pedigree">
				{#each drawing.gens as g (g.label)}<text x="16" y={g.y + 4} class="gen">{g.label}</text>{/each}
				{#each drawing.segs as s, i (i)}<line x1={s.x1} y1={s.y1} x2={s.x2} y2={s.y2} class="ln" />{/each}
				{#each drawing.nodes as b (b.id ?? 'proband')}
					{#if editable(b)}
						<g class="node edit" class:link={b.linked} class:masked={b.masked} class:aff={b.affected} class:pat={!!b.id && shade.has(b.id)} role="button" tabindex="0" aria-label={b.title} data-id={b.id} onclick={() => openEdit(b.id, true)} onkeydown={(e) => keyOpen(e, b)}>
							{@render symbol(b)}
						</g>
					{:else}
						<g class="node" class:link={b.linked} class:masked={b.masked} class:aff={b.affected} class:pat={!!b.id && shade.has(b.id)} role="img" aria-label={b.title} data-id={b.id}>
							{@render symbol(b)}
						</g>
					{/if}
				{/each}
			</svg>
		</div>
		{#if wide}<div class="dim hint swipe">{t('fam_swipe')}</div>{/if}
		{#if ped.data?.nodes.length}<div class="legend dim">
			<span><svg width="14" height="14" aria-hidden="true"><rect x="1" y="1" width="12" height="12" class="sym aff" /></svg>{t('fam_lg_affected')}</span>
			<span><svg width="14" height="14" aria-hidden="true"><rect x="2" y="2" width="10" height="10" class="sym" /><line x1="0" y1="14" x2="14" y2="0" class="dead" /></svg>{t('fam_lg_deceased')}</span>
			<span><svg width="14" height="14" aria-hidden="true"><path d="M1 13 L11 3 M11 3 l-5 0.5 M11 3 l-0.5 5" class="arrow" /></svg>{t('fam_proband')}</span>
			<span><svg width="14" height="14" aria-hidden="true"><circle cx="7" cy="7" r="5.5" class="sym link" /></svg>{t('fam_lg_linked')}</span>
			{#if shade.size}<span><svg width="14" height="14" aria-hidden="true"><circle cx="7" cy="7" r="5.5" class="sym patsym" /></svg>{t('fam_lg_pattern')}</span>{/if}
		</div>{:else}<div class="empty">{t('fam_empty')}</div>{/if}
	{/if}

	{#if items.length}
		<ul class="rel" aria-label={t('fam_list')}>
			{#each items as m (m.id)}
				<li>
					{#if canWrite}
						<button class="linkish" onclick={() => openEdit(m.id)} title={m.masked ? t('fam_masked_d') : tf('fam_edit', { rel: relLabel(m.relationship) })}>{listLine(m)}</button>
					{:else}
						<span title={m.masked ? t('fam_masked_d') : undefined}>{listLine(m)}</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
	{#if fam.error && !fam.data && !ped.error}<NotReady error={fam.error} onretry={load} />{/if}

	{#if draft && catalog}
		<form class="p2form drawer" onsubmit={save} aria-labelledby="fam-h" use:focusOnOpen={{ oncancel: close }}>
			<h3 id="fam-h">{editing ? tf('fam_edit', { rel: relLabel(editing.relationship) }) : t('fam_new')}</h3>
			<div class="p2row">
				<label>{t('fam_rel')}
					<select class="p2in" bind:value={draft.relationship}>
						{#each catalog.relationships as r (r.code)}<option value={r.code}>{cap(r[lang])}</option>{/each}
					</select>
				</label>
				<label class="wide">{t('fam_name')}<input class="p2in" bind:value={draft.display_name} maxlength="80" autocomplete="off" /></label>
				<label>{t('i2_sex')}
					<select class="p2in" bind:value={draft.sex_at_birth}>
						<option value="">—</option><option value="female">{t('i2_sex_f')}</option><option value="male">{t('i2_sex_m')}</option><option value="unknown">{t('i2_sex_u')}</option>
					</select>
				</label>
			</div>
			<div class="p2row">
				<label>{t('fam_born')}<input class="p2in num" type="number" min="1800" max={new Date().getFullYear()} bind:value={draft.born_year} /></label>
				<label class="check"><input type="checkbox" bind:checked={draft.deceased} /> {t('fam_deceased')}</label>
				{#if draft.deceased}<label>{t('fam_death_age')}<input class="p2in num" type="number" min="0" max="130" bind:value={draft.death_age} /></label>{/if}
				<label>{t('fam_link')}
					<select class="p2in" bind:value={draft.linked_profile}>
						<option value="">{t('fam_link_none')}</option>
						{#each linkable as p (p.slug)}<option value={p.slug}>{p.display_name}</option>{/each}
					</select>
				</label>
			</div>
			<fieldset>
				<legend>{t('fam_conds')}</legend>
				{#each draft.conditions as c, i (i)}
					<div class="p2row cond">
						<select class="p2in" aria-label={t('fam_conds')} value={c.kind} onchange={(e) => setKind(c, e.currentTarget.value)}>
							{#each catalog.conditions as k (k.kind)}<option value={k.kind}>{cap(k[lang])}</option>{/each}
						</select>
						{#if c.kind === 'other'}<input class="p2in" aria-label={t('fam_cond_text')} placeholder={t('fam_cond_text')} bind:value={c.text} maxlength="200" required />{/if}
						<label class="inline">{t('fam_onset')}<input class="p2in num" type="number" min="0" max="130" bind:value={c.onset_age} /></label>
						<label class="check"><input type="checkbox" bind:checked={c.onset_approx} disabled={c.onset_age == null} /> {t('fam_approx')}</label>
						<button class="btn" type="button" aria-label={t('fam_cond_remove')} title={t('fam_cond_remove')} onclick={() => draft?.conditions.splice(i, 1)}>✕</button>
					</div>
				{/each}
				<button class="btn" type="button" onclick={() => draft?.conditions.push({ kind: 'mi', text: null, onset_age: null, onset_approx: false })}>{t('fam_cond_add')}</button>
			</fieldset>
			{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
			{#if confirming && editing}
				<ConfirmRow text={tf('fam_remove_q', { rel: cap(relLabel(editing.relationship)) })} confirmLabel={t('fam_remove_ok')} onconfirm={remove} oncancel={() => (confirming = false)} {busy} />
			{:else}
				<div class="p2row">
					<button class="btn pri" type="submit" disabled={busy}>{t('fam_save')}</button>
					<button class="btn" type="button" onclick={close}>{t('i2_cancel')}</button>
					{#if editing}<span class="grow"></span><button class="btn danger" type="button" onclick={() => (confirming = true)}>{t('fam_remove')}</button>{/if}
				</div>
			{/if}
		</form>
	{:else if editing?.masked}
		<div class="p2form drawer" role="group" aria-labelledby="fam-h" use:focusOnOpen={{ oncancel: close }}>
			<h3 id="fam-h">{cap(relLabel(editing.relationship))} · {t('fam_masked')}</h3>
			<p class="dim">{t('fam_masked_d')} {t('fam_masked_rm')}</p>
			{#if error}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
			{#if confirming}
				<ConfirmRow text={tf('fam_remove_q', { rel: cap(relLabel(editing.relationship)) })} confirmLabel={t('fam_remove_ok')} onconfirm={remove} oncancel={() => (confirming = false)} {busy} />
			{:else}
				<div class="p2row">
					<button class="btn danger" type="button" onclick={() => (confirming = true)}>{t('fam_remove')}</button>
					<button class="btn" type="button" onclick={close}>{t('i2_cancel')}</button>
				</div>
			{/if}
		</div>
	{:else if error}
		<div class="p2err" role="alert" title={error.raw}>{error.text}</div>
	{/if}
	<div class="note">{t('fam_pat_note')}</div>
	{#if ped.data}<div class="mono dim rules">{ped.data.rules}</div>{/if}
</div>
{#if patterns && ped.data?.nodes.length}<FamilyPatterns {patterns} {relLabel} {kindLabel} bind:selected={pattern} />{/if}
{#if patterns?.hints?.length}<FamilyHints hints={patterns.hints} {relLabel} />{/if}

<style>
	.rules {
		font-size: 10.5px;
	}
	.tools {
		margin-bottom: 6px;
	}
	.hide {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
	}
	.hint {
		font-size: 11.5px;
	}
	.grow {
		flex: 1;
	}
	.svgwrap {
		overflow-x: auto;
	}
	.swipe {
		margin-top: 2px;
	}
	.svgwrap svg {
		display: block;
		/* fits the card when it nearly does; the inline min-width keeps a big family at 80% or more, and it scrolls */
		max-width: 100%;
		height: auto;
	}
	.gen {
		fill: var(--dim);
		font-size: 11px;
	}
	.ln {
		stroke: var(--muted);
		stroke-width: 1.6;
		pointer-events: none;
	}
	.sym {
		fill: var(--raised);
		stroke: var(--muted);
		stroke-width: 1.6;
	}
	.aff .sym,
	.sym.aff {
		fill: #4a2a30;
	}
	.link .sym,
	.sym.link {
		stroke: var(--accent);
		stroke-width: 2.6;
	}
	/* the relatives of the picked pattern: a warm ring, the affected fill kept */
	.pat .sym,
	.sym.patsym {
		stroke: var(--watch);
		stroke-width: 3.5;
	}
	.masked .sym {
		stroke-dasharray: 4 3;
		fill: var(--surface);
	}
	.dead {
		stroke: var(--text);
		stroke-width: 1.6;
	}
	.arrow {
		stroke: var(--accent);
		stroke-width: 2;
		fill: none;
	}
	.node text {
		text-anchor: middle;
		paint-order: stroke;
		stroke: var(--surface);
		stroke-width: 4px;
		stroke-linejoin: round;
	}
	.l1 {
		fill: var(--text);
		font-size: 12px;
		font-weight: 600;
	}
	.l2 {
		fill: var(--muted);
		font-size: 10.5px;
	}
	.node.edit {
		cursor: pointer;
	}
	.node.edit:hover .sym {
		stroke: var(--text);
	}
	.node:focus-visible {
		outline: none;
	}
	.node:focus-visible .sym {
		stroke: var(--accent);
		stroke-width: 3.5;
	}
	.legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
		font-size: 11.5px;
		margin: 4px 0 6px;
	}
	.legend span {
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}
	.rel {
		list-style: none;
		margin: 6px 0;
		padding: 0;
		display: grid;
		gap: 2px;
		font-size: 12.5px;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 2px 0;
		color: var(--text);
		text-align: left;
		cursor: pointer;
		font: inherit;
	}
	.linkish:hover {
		color: var(--accent);
	}
	.drawer {
		border-top: 1px solid var(--line);
		margin-top: 8px;
		padding-top: 10px;
	}
	.drawer h3 {
		margin: 0;
	}
	.wide {
		flex: 1;
		min-width: 160px;
	}
	.num {
		width: 84px;
	}
	label.check,
	label.inline {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	fieldset {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 6px 10px 10px;
		display: grid;
		gap: 6px;
		margin: 0;
	}
	legend {
		font-size: 11.5px;
		color: var(--muted);
		padding: 0 4px;
	}
	.danger {
		color: var(--out);
	}
	@media (max-width: 760px) {
		.hint {
			flex-basis: 100%;
		}
		.drawer .p2row > label:not(.check) {
			flex: 1 1 100%;
		}
		.drawer select.p2in {
			width: 100%;
		}
	}
</style>
