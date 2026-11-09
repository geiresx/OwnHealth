<script lang="ts">
	// Mockup screen #import-review: inbox tiles, upload, and the split view (document page with the
	// extracted rows boxed on the left, the rows waiting for a person on the right).
	import '#lib/components/pages2/pages2.css';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import {
		acceptReview,
		addAlias,
		bulkAccept,
		dismissReview,
		documentFileUrl,
		getDocument,
		listDocuments,
		listInboxes,
		listJobs,
		listReview,
		rejectReview,
		retractDocument
	} from '#lib/api';
	import type { DocumentDetail, DocumentSummary, JobInfo, ReviewItem } from '#lib/api/types';
	import type { InboxSource } from '#lib/api/inbox';
	import AgentContext from '#lib/components/inbox/AgentContext.svelte';
	import { nearDuplicateOf, paperlessLink, rowGrounding, sourceStatus } from '#lib/logic/inbox';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import PageView, { type Box } from '#lib/components/pages2/PageView.svelte';
	import ReviewRow from '#lib/components/pages2/ReviewRow.svelte';
	import UploadPanel from '#lib/components/pages2/UploadPanel.svelte';
	import ManualEntry from '#lib/components/ManualEntry.svelte';
	import { ago, day, plain } from '#lib/components/pages2/fmt';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { app, currentProfile } from '#lib/state/app.svelte';

	const THRESHOLD = 0.95;
	const prof = $derived(currentProfile());
	const queue = new Loader<{ items: ReviewItem[]; docs: DocumentSummary[]; jobs: JobInfo[]; sources: InboxSource[] }>();
	// "Not a duplicate? reject" waits for a second click: it retracts this document.
	let confirmDup = $state(false);
	const doc = new Loader<DocumentDetail>();
	let showUpload = $state(false);
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let selected = $state<string | null>(null);
	let pageNo = $state(1);
	let zoom = $state(1);
	let checked = $state<Record<string, boolean>>({});
	let rejectingDoc = $state(false);
	let docReason = $state('');
	// A photo or scanned PDF (pending_document): its values are typed in by hand, then the item closes.
	let entering = $state<string | null>(null);
	const docName = (id: string | null) => {
		const d = (queue.data?.docs ?? []).find((x) => x.id === id);
		return d ? docLabel(d) : '';
	};

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		queue.run(async () => {
			const [r, d, j, s] = await Promise.all([
				listReview({ status: 'open', profile: slug }),
				listDocuments(slug).catch(() => ({ items: [] as DocumentSummary[] })),
				listJobs().catch(() => ({ jobs: [] as JobInfo[] })),
				// The inbox tiles; a viewer who cannot manage sources (403) just sees the setup tile.
				listInboxes().catch(() => ({ items: [] as InboxSource[] }))
			]);
			if (app.me) app.me.review_open = r.open;
			return { items: r.items, docs: d.items, jobs: j.jobs, sources: s.items };
		}, slug);
	}
	$effect(() => {
		void prof?.slug;
		load();
	});

	const items = $derived(queue.data?.items ?? []);
	const docsWithRows = $derived.by(() => {
		const ids = new Set(items.filter((i) => i.kind === 'candidate_row' && i.document_id).map((i) => i.document_id as string));
		return (queue.data?.docs ?? []).filter((d) => ids.has(d.id));
	});
	// Links from MCP: get_source.crop_url (?doc=&page=&src=) and propose.review_url (?item=).
	const itemParam = $derived(page.url.searchParams.get('item'));
	const docId = $derived(
		page.url.searchParams.get('doc') ?? items.find((i) => i.id === itemParam)?.document_id ?? docsWithRows[0]?.id ?? null
	);
	$effect(() => {
		const id = docId;
		if (!id) return;
		selected = itemParam;
		checked = {};
		pageNo = Number(page.url.searchParams.get('page')) || 1;
		doc.run(() => getDocument(id), id);
	});
	const rows = $derived(items.filter((i) => i.kind === 'candidate_row' && i.document_id === docId));
	// v0.2: "probably the same as document X" (dedup level 2) is a banner over that document's rows.
	const near = $derived(nearDuplicateOf(items, docId));
	const others = $derived(items.filter((i) => !(i.kind === 'candidate_row' && i.document_id === docId) && i.id !== near?.id));
	const nearOf = $derived.by(() => {
		const id = (near?.payload as { same_as?: string } | undefined)?.same_as;
		return id ? { id, doc: (queue.data?.docs ?? []).find((x) => x.id === id) ?? null } : null;
	});
	const docNameOf = (id: string) => {
		const d = (queue.data?.docs ?? []).find((x) => x.id === id);
		return d ? docLabel(d) : '';
	};
	const pg = (i: ReviewItem) => Number((i.payload as { page_no?: number }).page_no ?? 1);
	const pinfo = $derived(doc.data?.pages_info.find((p) => p.page_no === pageNo) ?? null);
	const boxes = $derived<Box[]>(
		rows
			.filter((i) => pg(i) === pageNo && Array.isArray((i.payload as { bbox?: number[] }).bbox))
			.map((i) => ({
				id: i.id,
				bbox: (i.payload as { bbox: number[] }).bbox,
				tone: i.reasons.includes('possible_injection') || rowGrounding(i) === 'ungrounded' ? 'bad' : i.reasons.length ? 'low' : '',
				label: String(((i.payload as { observation?: { printed_name?: string } }).observation ?? {}).printed_name ?? '')
			}))
	);
	// Pages carrying text aimed at an AI or hidden text (lane B's flags on the page and on the document).
	const flagged = $derived.by(() => {
		const d = doc.data;
		if (!d) return [] as number[];
		const pages = new Set(d.pages_info.filter((p) => p.flags && ('possible_injection' in p.flags || 'hidden_text' in p.flags)).map((p) => p.page_no));
		for (const n of (d.flags?.hidden_text as number[] | undefined) ?? []) pages.add(n);
		return [...pages].sort((a, b) => a - b);
	});
	const paperlessUrl = $derived(paperlessLink(doc.data?.flags));
	const untrusted = $derived(!!doc.data?.flags?.possible_injection || flagged.length > 0 || rows.some((r) => r.reasons.includes('possible_injection')));
	const duplicate = $derived(rows.flatMap((i) => ((i.payload as { checks?: { reason: string | null; detail: string | null }[] }).checks ?? []).filter((c) => c.reason === 'duplicate'))[0] ?? null);
	const bulkable = $derived(rows.filter((i) => !i.reasons.length && (i.row_conf ?? 0) >= THRESHOLD).length);
	const picked = $derived(rows.filter((i) => checked[i.id]).map((i) => i.id));
	// Only this document's rows: the footer must not claim an agent proposed rows a PDF import produced.
	const agentCount = $derived(
		rows.filter((i) => i.proposed_by_token != null).length + items.filter((i) => i.kind === 'agent_proposal' && i.document_id === docId).length
	);

	function pick(id: string) {
		selected = id;
		const it = rows.find((i) => i.id === id);
		if (it) pageNo = pg(it);
		requestAnimationFrame(() => document.querySelector('.box.on')?.scrollIntoView({ block: 'nearest', inline: 'nearest' }));
		document.getElementById(`rr-${id.slice(0, 8)}-n`)?.scrollIntoView({ block: 'nearest' });
	}

	async function act(fn: () => Promise<unknown>) {
		busy = true;
		actionError = null;
		try {
			await fn();
		} catch (e) {
			actionError = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
			// Also after a failure: a batch that failed at row N already accepted rows 1 to N-1.
			load();
		}
	}
	const acceptPicked = () => act(async () => { for (const id of picked) await acceptReview(id); });

	// Inbox tiles: what each channel left in the queue, and its newest import.
	const latest = (kind: string) => (queue.data?.jobs ?? []).find((j) => j.kind === kind) ?? null;
	const latestDoc = (type: string) => (queue.data?.docs ?? []).find((d) => d.doc_type === type) ?? null;
	const count = (...kinds: string[]) => items.filter((i) => kinds.includes(i.kind)).length;
	const tiles = $derived([
		{ k: 'i2_in_lab', d: latest('import.pdf'), doc: latestDoc('lab_report'), n: count('candidate_row', 'document_identity', 'amended_report') },
		{ k: 'i2_in_apple', d: latest('import.apple_health'), n: null },
		{ k: 'i2_in_fhir', d: latest('import.fhir_bundle'), n: count('fhir_unmapped') },
		{ k: 'i2_in_genome', d: latest('import.genome_facts'), doc: latestDoc('genomics'), n: count('genome_reinterpretation') },
		{ k: 'i2_in_agent', d: null, n: count('agent_proposal'), sub: 'write:propose' },
	]);
	// v0.2: one tile per configured inbox, so the one that brought items in or is failing shows at a
	// glance; a mailbox counts its held mail (the server's count). No inbox yet: one setup tile.
	const inboxTiles = $derived(
		(queue.data?.sources ?? []).map((s) => {
			const c = s.config as Record<string, unknown>;
			const what =
				s.kind === 'email' ? String(c.username ?? c.host ?? '') : s.kind === 'folder' ? `/${String(c.subdir ?? c.path ?? '')}` : s.kind === 'paperless' && c.tag ? `tag: ${String(c.tag)}` : '';
			const st = sourceStatus(s);
			const bad = st === 'out' || st === 'partial';
			const state = st === 'out' ? tf('fu_tile_failing', { n: 1 }) : st === 'partial' ? t('fu_st_partial') : s.last_success_at ? tf('fu_tile_synced', { when: ago(s.last_success_at) }) : t('fu_st_nodata');
			return { s, sub: [t('fu_k_' + s.kind), what, state].filter(Boolean).join(' · '), bad, held: s.kind === 'email' ? (s.quarantined ?? 0) : null };
		})
	);
	function jobLine(j: JobInfo | null, d?: DocumentSummary | null): string {
		const doc = j?.document_id ? (queue.data?.docs ?? []).find((x) => x.id === j.document_id) : d;
		const name = j?.filename ?? (doc ? plain(doc.filename) : null);
		const when = j ? day(j.finished_at ?? j.created_at) : doc?.collected_at ? day(doc.collected_at) : null;
		return name || when ? [name, when].filter(Boolean).join(' · ') : t('i2_in_none');
	}

	const docLabel = (d: DocumentSummary) => `${plain(d.filename) || d.provider_name || d.id.slice(0, 8)}`;
	function otherTitle(i: ReviewItem): string {
		const p = i.payload as Record<string, unknown>;
		if (i.kind === 'agent_proposal') return tf('i2_ag_t', { kind: String(p.kind ?? ''), token: String(p.token_label ?? '—') });
		if (i.kind === 'job_failed') return tf('i2_jf_t', { file: String(p.filename ?? p.kind ?? '') });
		if (i.kind === 'candidate_row') {
			const d = (queue.data?.docs ?? []).find((x) => x.id === i.document_id);
			return tf('i2_cr_t', { name: String((p.observation as { printed_name?: string })?.printed_name ?? ''), doc: d ? docLabel(d) : '—' });
		}
		return t('i2_kind_' + i.kind);
	}
	function otherText(i: ReviewItem): string {
		const p = i.payload as Record<string, unknown>;
		const inner = p.payload as Record<string, unknown> | undefined;
		if (i.kind === 'agent_proposal') return String(inner?.text ?? JSON.stringify(inner ?? {}));
		if (i.kind === 'job_failed') return String(p.error ?? p.last_error ?? '');
		return '';
	}
</script>

<PageHeader title={t('nav_imp')} sub={t('i2_imp_sub')}>
	{#snippet right()}
		<PhaseChip phase={t('i2_imp_ph1')} /><PhaseChip phase={t('i2_imp_ph2')} />
		<button class="btn pri" onclick={() => (showUpload = !showUpload)} aria-expanded={showUpload}>{t('i2_upload')}</button>
	{/snippet}
</PageHeader>

{#if showUpload && prof}
	<UploadPanel profile={prof.slug} profileName={prof.display_name} ondone={load} />
{/if}

<div class="inbox">
	{#each tiles as b (b.k)}
		<div class="ib">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px">{t(b.k)}</div>
				<div class="dim" style="font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">{b.sub ?? jobLine(b.d, b.doc)}</div>
			</div>
			{#if b.n != null}<span class="c" style="color:{b.n ? 'var(--watch)' : 'var(--dim)'}" aria-label="{b.n} {t('i2_open_items')}">{b.n}</span>{/if}
		</div>
	{/each}
	{#each inboxTiles as b (b.s.id)}
		<a class="ib lnkt" href="/inbox">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">{b.s.label}</div>
				<div class="dim" style="font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;{b.bad ? 'color:var(--out)' : ''}">{b.sub}</div>
			</div>
			{#if b.held != null}<span class="c" style="color:{b.held ? 'var(--watch)' : 'var(--dim)'}" title={t('fu_quarantine')} aria-label="{b.held} {t('fu_quarantine')}">{b.held}</span>{/if}
		</a>
	{:else}
		<a class="ib lnkt" href="/inbox">
			<div style="min-width:0">
				<div style="font-weight:600;font-size:12.5px">{t('fu_inbox')} <PhaseChip phase="v0.2" /></div>
				<div class="dim" style="font-size:11px">{t('fu_tile_setup')}</div>
			</div>
		</a>
	{/each}
</div>

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px">{actionError}</div>{/if}

{#if queue.error}
	<NotReady error={queue.error} onretry={load} />
{:else if queue.loading && !queue.data}
	<div class="empty">{t('loading')}</div>
{:else if !docId}
	<div class="card"><div class="empty">{items.length ? t('i2_no_doc_rows') : t('i2_queue_empty')} <a href="/documents">{t('i2_docs_link')}</a></div></div>
{:else}
	<div class="imp">
		<div>
			<div style="display:flex;gap:8px;align-items:center;margin-bottom:8px">
				{#if docsWithRows.length > 1}
					<select class="p2in" style="max-width:220px;font-weight:600" aria-label={t('i2_doc')} value={docId} onchange={(e) => goto(`?doc=${(e.currentTarget as HTMLSelectElement).value}`)}>
						{#each docsWithRows as d (d.id)}<option value={d.id}>{docLabel(d)}</option>{/each}
					</select>
				{:else if doc.data}
					<b style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">{plain(doc.data.filename)}</b>
				{/if}
				{#if doc.data}
					<span class="dim" style="font-size:12px;white-space:nowrap">{[doc.data.provider_name, day(doc.data.collected_at), pinfo ? (pinfo.text_layer_ok ? t('i2_textlayer') : t('i2_ocr')) : null].filter(Boolean).join(' · ')}</span>
					<button class="btn sm" style="margin-left:auto" onclick={() => (zoom = zoom >= 2 ? 1 : zoom + 0.5)} title={t('i2_zoom')} aria-label="{t('i2_zoom')} {zoom}×">{zoom}×</button>
					<span class="seg" role="group" aria-label={t('i2_page')}>
						{#each doc.data.pages_info as p (p.page_no)}
							<button class:on={p.page_no === pageNo} aria-pressed={p.page_no === pageNo} onclick={() => (pageNo = p.page_no)}>p{p.page_no}{#if flagged.includes(p.page_no)} <span aria-label={t('i2_flag_possible_injection')}>⚠</span>{/if}</button>
						{/each}
					</span>
				{/if}
			</div>
			{#if doc.error}
				<NotReady error={doc.error} />
			{:else if doc.data && pinfo && docId}
				<PageView documentId={docId} page={pageNo} width={pinfo.width_pt} height={pinfo.height_pt} {boxes} {selected} {zoom} onpick={pick} />
			{:else if doc.data && !doc.data.pages_info.length}
				<div class="empty">{t('i2_no_pages')}</div>
			{:else}
				<div class="empty">{t('loading')}</div>
			{/if}
		</div>

		<div class="card" style="padding:0;overflow:hidden;align-self:start">
			<div style="display:flex;align-items:center;gap:8px;padding:10px 12px;border-bottom:1px solid var(--line)">
				<b style="white-space:nowrap">{t('i2_rows')}</b>
				<span class="dim" style="font-size:12px;white-space:nowrap">{tf('i2_rows_need', { n: rows.length })}</span>
				<span style="margin-left:auto;display:flex;gap:6px">
					{#if paperlessUrl}<a class="btn sm" href={paperlessUrl} target="_blank" rel="noopener noreferrer">{t('fu_open_paperless')} ↗</a>{/if}
					{#if picked.length}<button class="btn sm ok" onclick={acceptPicked} disabled={busy}>{tf('i2_accept_sel', { n: picked.length })}</button>{/if}
					<button class="btn sm" onclick={() => docId && act(() => bulkAccept(docId, THRESHOLD))} disabled={busy || !bulkable} title={t('i2_bulk_d')}>{t('i2_bulk')} ≥ {THRESHOLD.toFixed(2)}</button>
					<button class="btn sm no" onclick={() => (rejectingDoc = !rejectingDoc)} aria-expanded={rejectingDoc}>{t('i2_reject_doc')}</button>
				</span>
			</div>
			{#if rejectingDoc}
				<form class="p2row" style="padding:8px 12px;border-bottom:1px solid var(--line)" onsubmit={(e) => { e.preventDefault(); if (docId) act(() => retractDocument(docId, docReason.trim())); rejectingDoc = false; }}>
					<span class="muted" style="font-size:12px">{t('i2_reject_doc_d')}</span>
					<input class="p2in" style="flex:1" bind:value={docReason} required placeholder={t('i2_reject_reason')} aria-label={t('i2_reject_reason')} />
					<button class="btn sm no" type="submit" disabled={busy}>{t('i2_reject_doc')}</button>
				</form>
			{/if}
			{#if near && docId}
				<div class="banner dupb" role="note" style="border-color:var(--line2);background:var(--raised)">
					<span class="muted" style="font-size:12px">{t('i2_dup')} {nearOf?.doc ? tf('fu_dup_doc', { doc: docLabel(nearOf.doc), date: day(nearOf.doc.collected_at) }) : t('fu_dup_unknown')} {tf('fu_dup_pages', { n: Number((near.payload as { pages?: number }).pages ?? 0) })}</span>
					<span class="p2row" style="margin-left:auto;gap:6px">
						{#if nearOf}<a class="btn sm" href="?doc={nearOf.id}">{t('fu_dup_compare')}</a>{/if}
						<button class="btn sm" disabled={busy} onclick={() => near && act(() => dismissReview(near.id, 'kept both'))}>{t('fu_dup_keep')}</button>
						{#if confirmDup}
							<button class="btn sm no" disabled={busy} onclick={() => act(async () => { confirmDup = false; await retractDocument(docId, `duplicate of ${nearOf?.id ?? 'an earlier document'}`); if (near) await dismissReview(near.id, 'duplicate retracted'); })}>{t('fu_dup_reject_yes')}</button>
							<button class="btn sm" onclick={() => (confirmDup = false)}>{t('i2_cancel')}</button>
						{:else}
							<button class="btn sm no" disabled={busy} onclick={() => (confirmDup = true)}>{t('fu_dup_reject')}</button>
						{/if}
					</span>
				</div>
				{#if confirmDup}<div class="note" style="margin:0 12px 8px">{t('fu_dup_reject_d')}</div>{/if}
			{/if}
			{#if duplicate}
				<div class="banner" style="margin:10px 12px;padding:7px 10px;border-color:var(--line2);background:var(--raised)">
					<span class="muted" style="font-size:12px">{t('i2_dup')} {duplicate.detail ?? ''}</span>
				</div>
			{/if}
			<div class="xr h" aria-hidden="true"><span></span><span>{t('i2_h_name')}</span><span>{t('i2_h_loinc')}</span><span>{t('i2_h_value')}</span><span>{t('i2_h_conf')}</span><span style="text-align:right">{t('i2_h_act')}</span></div>
			{#each rows as it (it.id)}
				<ReviewRow
					item={it}
					threshold={THRESHOLD}
					selected={selected === it.id}
					checked={!!checked[it.id]}
					oncheck={(on) => (checked[it.id] = on)}
					{busy}
					docName={docNameOf}
					onselect={() => pick(it.id)}
					onaccept={(edits) => act(() => acceptReview(it.id, edits))}
					onreject={(reason) => act(() => rejectReview(it.id, reason))}
					ondiscard={() => act(() => dismissReview(it.id, 'untrusted text'))}
					onalias={(b) => prof && act(() => addAlias({ profile: prof.slug, alias: String((it.payload as { observation?: { printed_name?: string } }).observation?.printed_name ?? ''), alias_group: b.alias_group, loinc: b.loinc, lang: i18n.lang }))}
				/>
			{:else}
				<div class="empty">{t('i2_doc_done')}</div>
			{/each}
			<div style="padding:10px 12px;display:flex;flex-direction:column;gap:6px">
				{#if agentCount}
					<div class="muted" style="font-size:11.5px"><span class="chip" style="color:var(--accent);border-color:rgba(110,168,254,.5)">agent</span> {t('i2_agent_note')}</div>
				{/if}
				{#if untrusted}
					<div class="muted" style="font-size:11.5px" role="note">
						<span class="chip" style="color:var(--out);border-color:rgba(239,107,115,.5)">untrusted</span>
						{flagged.length ? tf('i2_untrusted_note', { pages: flagged.join(', ') }) : t('i2_untrusted_doc')} <span class="mono">trust: untrusted_text</span>{t('i2_untrusted_note2')}
					</div>
				{/if}
			</div>
		</div>
	</div>
{/if}

{#if others.length}
	<div class="card" style="margin-top:12px;padding:8px 6px 2px">
		<h3 style="padding:0 8px">{t('i2_others')}<span class="r">{tf('i2_others_d', { n: others.length })}</span></h3>
		<table class="t">
			<tbody>
				{#each others as it (it.id)}
					<tr class:agent={it.kind === 'agent_proposal'}>
						<td style="width:1%"><span class="chip mono" style={it.kind === 'agent_proposal' ? 'color:var(--accent);border-color:rgba(110,168,254,.5)' : ''}>{it.kind}</span></td>
						<td>
							<div>{it.kind === 'agent_proposal' ? '◆ ' : ''}{otherTitle(it)}</div>
							{#if otherText(it)}<pre class="p2untrusted" style="margin-top:4px">{otherText(it)}</pre>{/if}
							{#if it.kind === 'agent_proposal'}<AgentContext item={it} docName={docNameOf} />{:else if it.context_src_ids?.length}<div class="dim mono" style="font-size:10.5px">{t('i2_based_on')} {it.context_src_ids.join(', ')}</div>{/if}
						</td>
						<td class="dim num" style="white-space:nowrap">{day(it.created_at)}</td>
						<td style="white-space:nowrap;text-align:right">
							{#if it.kind === 'candidate_row' && it.document_id}
								<a class="btn sm" href="?doc={it.document_id}">{t('i2_open_doc')}</a>
							{:else if it.kind === 'pending_document' && prof}
								{#if it.document_id}<a class="btn sm" href={documentFileUrl(it.document_id)} target="_blank" rel="noopener">{t('i2_open_doc')}</a>{/if}
								<button class="btn sm pri" onclick={() => (entering = entering === it.id ? null : it.id)} aria-expanded={entering === it.id}>{t('me_enter')}</button>
								<button class="btn sm" onclick={() => act(() => dismissReview(it.id, entering === it.id ? 'entered by hand' : undefined))} disabled={busy}>{entering === it.id ? t('me_done') : t('i2_dismiss')}</button>
							{:else if it.kind === 'agent_proposal' || it.kind === 'fhir_unmapped'}
								<button class="btn sm ok" onclick={() => act(() => acceptReview(it.id))} disabled={busy}>{t('i2_accept')}</button>
								<button class="btn sm no" onclick={() => act(() => rejectReview(it.id, 'rejected in review'))} disabled={busy}>{t('i2_reject')}</button>
							{:else}
								<button class="btn sm" onclick={() => act(() => dismissReview(it.id))} disabled={busy}>{t('i2_dismiss')}</button>
							{/if}
						</td>
					</tr>
					{#if entering === it.id && prof}
						<tr><td colspan="4">
							<ManualEntry profile={prof.slug} kinds={['observation', 'medication', 'immunization']} note={tf('me_from_doc', { doc: docName(it.document_id) || '—' })} onsaved={() => null} />
						</td></tr>
					{/if}
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	.seg button {
		white-space: nowrap;
	}
	tr.agent td {
		background: rgba(110, 168, 254, 0.05);
	}
	.imp {
		grid-template-columns: 430px minmax(0, 1fr);
		align-items: start;
	}
	.dupb {
		margin: 10px 12px;
		padding: 7px 10px;
		flex-wrap: wrap;
	}
	@media (max-width: 760px) {
		.imp {
			grid-template-columns: minmax(0, 1fr);
		}
		.inbox {
			flex-wrap: wrap;
		}
		.inbox .ib {
			flex: 1 1 45%;
		}
	}
	a.lnkt {
		color: inherit;
	}
	a.lnkt:hover {
		border-color: var(--line2);
	}
</style>
