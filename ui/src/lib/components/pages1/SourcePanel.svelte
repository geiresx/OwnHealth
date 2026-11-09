<script lang="ts">
	// Where a value came from: GET /api/sources/{id}, and the page image with the value's box when the
	// document service can render it. Opened by any SourceChip through openSource().
	import { getDocument, getSource, pageImageUrl } from '#lib/api';
	import type { DocumentDetail, SourceDetail } from '#lib/api/types';
	import { t } from '#lib/i18n/index.svelte';
	import { paperlessLink } from '#lib/logic/inbox';
	import NotReady from './NotReady.svelte';
	import { sourcePanel } from './source.svelte';
	import { Loader } from './util.svelte';

	let dialog: HTMLDialogElement | undefined = $state();
	const src = new Loader<SourceDetail>();
	let doc = $state<DocumentDetail | null>(null);
	let imgFailed = $state(false);

	$effect(() => {
		const id = sourcePanel.src;
		if (!id || !dialog) return;
		doc = null;
		imgFailed = false;
		if (!dialog.open) dialog.showModal();
		src.run(async () => {
			const s = await getSource(id);
			// The page size comes from the document; drop it if another source was opened meanwhile.
			const current = () => sourcePanel.src === id;
			if (s.document && s.location)
				getDocument(s.document.id)
					.then((d) => current() && (doc = d))
					.catch(() => current() && (doc = null));
			return s;
		}, id);
	});

	function close() {
		sourcePanel.src = null;
		dialog?.close();
	}

	const box = $derived.by(() => {
		const s = src.data;
		const bbox = s?.location?.bbox;
		const page = doc?.pages_info.find((p) => p.page_no === s?.location?.page);
		if (!bbox || !page) return null;
		const [x0, y0, x1, y1] = bbox;
		const pct = (v: number, of: number) => `${((v / of) * 100).toFixed(2)}%`;
		return { left: pct(x0, page.width_pt), top: pct(y0, page.height_pt), width: pct(x1 - x0, page.width_pt), height: pct(y1 - y0, page.height_pt) };
	});
	const day = (v: string | null | undefined) => (v ? v.slice(0, 10) : '—');
</script>

<dialog bind:this={dialog} class="srcp" onclose={() => (sourcePanel.src = null)} aria-labelledby="srcp-h">
	<div class="card" style="padding:12px 14px">
		<h3 id="srcp-h">
			{t('h1_src_t')} <span class="mono dim">{sourcePanel.src ?? ''}</span>
			<span class="r"><button class="btn sm" onclick={close}>{t('h1_close')}</button></span>
		</h3>
		{#if src.loading && !src.data}
			<div class="empty">{t('loading')}</div>
		{:else if src.error}
			<NotReady error={src.error} />
		{:else if src.data}
			{@const s = src.data}
			<table class="t" style="font-size:12px">
				<tbody>
					{#if s.label}<tr><td class="dim">{t('h1_src_t')}</td><td>{s.label}</td></tr>{/if}
					<tr><td class="dim">{t('h1_src_kind')}</td><td class="mono">{s.kind}</td></tr>
					<tr><td class="dim">{t('h1_src_status')}</td><td>{s.status}</td></tr>
					{#if s.document}
						<tr><td class="dim">{t('h1_src_doc')}</td><td>{[s.document.provider_name, s.document.doc_type, s.document.report_status].filter(Boolean).join(' · ')}</td></tr>
						<tr><td class="dim">{t('h1_src_collected')}</td><td class="num">{day(s.document.collected_at)}</td></tr>
					{/if}
					{#if s.location}<tr><td class="dim">{t('h1_src_page')}</td><td class="num">{s.location.page}{s.document?.pages ? ' / ' + s.document.pages : ''}</td></tr>{/if}
					{#if s.import_ref}<tr><td class="dim">{t('h1_src_ref')}</td><td class="mono" style="font-size:11px;word-break:break-all">{s.import_ref}</td></tr>{/if}
					{#if paperlessLink(doc?.flags)}<tr><td class="dim">Paperless</td><td><a href={paperlessLink(doc?.flags)} target="_blank" rel="noopener noreferrer">{t('fu_open_paperless')} ↗</a></td></tr>{/if}
				</tbody>
			</table>
			{#if s.status === 'gone'}<div class="note" style="margin-top:8px">{t('h1_src_gone')}</div>{/if}
			{#if s.document && s.location}
				{#if imgFailed}
					<div class="empty">{t('h1_src_noimg')}</div>
				{:else}
					<div class="pg">
						<img src={pageImageUrl(s.document.id, s.location.page)} alt="{t('h1_src_page')} {s.location.page}" onerror={() => (imgFailed = true)} />
						{#if box}<div class="bb" style="position:absolute;left:{box.left};top:{box.top};width:{box.width};height:{box.height}"></div>{/if}
					</div>
				{/if}
			{/if}
		{/if}
	</div>
</dialog>

<style>
	.srcp {
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--text);
		width: min(640px, 92vw);
		max-height: 88vh;
	}
	.srcp::backdrop {
		background: rgba(0, 0, 0, 0.55);
	}
	.pg {
		position: relative;
		margin-top: 10px;
		border-radius: 4px;
		overflow: hidden;
		background: #f4f2ee;
	}
	.pg img {
		display: block;
		width: 100%;
	}
</style>
