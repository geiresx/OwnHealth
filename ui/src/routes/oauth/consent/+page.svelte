<script lang="ts">
	// The OAuth consent screen (mcp.md 2.2, security-privacy.md 5). The authorization server sends the
	// browser here as /oauth/consent?request=<id> after it checked the client's metadata document, the
	// redirect URI, PKCE and the resource. Every profile and every requested scope is a checkbox in plain
	// words; nothing is pre-checked beyond read:summary, and no profile is. Approving needs step-up.
	import '#lib/components/pages2/pages2.css';
	import { page } from '$app/state';
	import { approveOAuth, denyOAuth, getOAuthRequest, type IdentityMode, type OAuthRequestOut } from '#lib/api/inbox';
	import type { Scope } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { plain } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { consentDefaults, consentProblem } from '#lib/logic/inbox';

	const RISKY: Scope[] = ['read:genome_raw', 'read:sensitive'];
	const req = new Loader<OAuthRequestOut>();
	const id = $derived(page.url.searchParams.get('request') ?? '');
	let scopes = $state<string[]>([]);
	let profiles = $state<string[]>([]);
	let days = $state(90);
	let identity = $state<IdentityMode>('pseudonymous');
	let error = $state<string | null>(null);
	let busy = $state(false);

	$effect(() => {
		const rid = id;
		if (!rid) return;
		req.run(async () => {
			const r = await getOAuthRequest(rid);
			const d = consentDefaults(r.scopes.map(scopeOf), r.default_days ?? 90);
			scopes = d.scopes;
			profiles = d.profiles;
			days = d.days;
			return r;
		}, rid);
	});

	type ReqScope = OAuthRequestOut['scopes'][number];
	function scopeOf(s: ReqScope): Scope {
		return typeof s === 'string' ? s : s.scope;
	}
	// The server's own wording when it sends one, else ours.
	function words(s: ReqScope): string {
		if (typeof s !== 'string' && s[i18n.lang]) return s[i18n.lang] as string;
		const k = 'fu_sc_' + scopeOf(s).replace(':', '_');
		return t(k) !== k ? t(k) : scopeOf(s);
	}
	const redirectHost = $derived.by(() => {
		try {
			return req.data ? new URL(req.data.redirect_uri).host : '';
		} catch {
			return req.data?.redirect_uri ?? '';
		}
	});

	async function approve(e: SubmitEvent) {
		e.preventDefault();
		const problem = consentProblem(scopes, profiles, days);
		if (problem) {
			error = t(problem);
			return;
		}
		busy = true;
		error = null;
		try {
			const r = await withStepUp(() =>
				approveOAuth(id, { scopes: scopes as Scope[], profile_ids: profiles, default_profile: profiles[0], identity_mode: identity, days })
			);
			location.href = r.redirect;
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
			busy = false;
		}
	}
	async function deny() {
		busy = true;
		try {
			location.href = (await denyOAuth(id)).redirect;
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
			busy = false;
		}
	}
</script>

<div class="wrap">
	{#if !id}
		<div class="card"><div class="empty">{t('fu_oa_missing')}</div></div>
	{:else if req.error}
		{#if 'status' in req.error && (req.error.status === 404 || req.error.status === 410)}
			<div class="card"><div class="empty">{t('fu_oa_gone')}</div></div>
		{:else}
			<NotReady error={req.error} />
		{/if}
	{:else if !req.data}
		<div class="empty">{t('loading')}</div>
	{:else}
		{@const r = req.data}
		<form class="card p2form" onsubmit={approve} aria-labelledby="oa-h">
			<div class="who">
				<!-- No client logo: it would be an outside request from this page, and its picture is untrusted. -->
				<div class="logo" aria-hidden="true"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="1.8"><rect x="4" y="11" width="16" height="10" rx="2" /><path d="M8 11V7a4 4 0 0 1 8 0v4" /></svg></div>
				<div style="min-width:0">
					<h1 id="oa-h">{tf('fu_oa_title', { client: plain(r.client_name) || redirectHost })}</h1>
					<div class="mono dim url" title={t('fu_oa_cimd_d')}>{r.client_id}</div>
				</div>
			</div>
			<div class="note">{t('fu_oa_cimd_d')} {tf('fu_oa_back', { host: redirectHost })}</div>

			<fieldset class="fs">
				<legend>{t('fu_oa_profiles')}</legend>
				{#each r.profiles as p (p.id)}
					<label class="ck"><input type="checkbox" value={p.id} bind:group={profiles} /> {p.display_name}</label>
				{:else}
					<span class="dim">{t('fu_oa_noprofiles')}</span>
				{/each}
			</fieldset>

			<fieldset class="fs col">
				<legend>{t('fu_oa_scopes')}</legend>
				{#each r.scopes as s (scopeOf(s))}
					<label class="ck">
						<input type="checkbox" value={scopeOf(s)} bind:group={scopes} />
						<span>{words(s)} <span class="chip mono" class:r={RISKY.includes(scopeOf(s))} class:w={scopeOf(s) === 'write:propose'}>{scopeOf(s)}</span></span>
					</label>
				{/each}
			</fieldset>

			<div class="p2row">
				<label>{t('i2_tok_days')}<input class="p2in num" type="number" min="1" max={r.max_days ?? 365} required bind:value={days} style="width:90px" /></label>
				<label
					>{t('i2_tok_identity')}
					<select class="p2in" bind:value={identity}>
						<option value="pseudonymous">{t('i2_id_pseudonymous')}</option>
						<option value="minimal">{t('i2_id_minimal')}</option>
						<option value="full">{t('i2_id_full')}</option>
					</select>
				</label>
			</div>
			<div class="note">{t('i2_tok_note')}</div>
			<div class="note">{t('fu_oa_revoke')}</div>
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<div class="p2row" style="justify-content:flex-end">
				<button type="button" class="btn no" disabled={busy} onclick={deny}>{t('fu_oa_deny')}</button>
				<button type="submit" class="btn pri" disabled={busy}>{t('fu_oa_approve')}</button>
			</div>
		</form>
	{/if}
</div>

<style>
	.wrap {
		max-width: 620px;
		margin: 10px auto;
	}
	h1 {
		font-size: 17px;
		font-weight: 620;
		margin: 0;
	}
	.who {
		display: flex;
		gap: 12px;
		align-items: center;
	}
	.logo {
		width: 36px;
		height: 36px;
		border-radius: 9px;
		flex: none;
		background: var(--accent-bg);
		display: grid;
		place-items: center;
	}
	.url {
		font-size: 11.5px;
		overflow-wrap: anywhere;
	}
	.fs {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 6px 10px 8px;
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
	}
	.fs.col {
		flex-direction: column;
	}
	.fs legend {
		font-size: 11.5px;
		color: var(--muted);
		padding: 0 4px;
	}
	.ck {
		display: inline-flex !important;
		align-items: center;
		gap: 6px;
		font-size: 12.5px !important;
		color: var(--text) !important;
	}
	.chip.w {
		color: var(--watch);
		border-color: rgba(224, 168, 74, 0.5);
	}
	.chip.r {
		color: var(--out);
		border-color: rgba(239, 107, 115, 0.5);
	}
</style>
