<script lang="ts">
	// Remote MCP clients you approved over OAuth (claude.ai and others): profiles, scopes, expiry, last
	// use, and revoke. Revoking only narrows access, so it needs no step-up.
	import { listConsents, revokeConsent, type OAuthConsentOut } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { ago, day, plain } from '#lib/components/pages2/fmt';
	import { t } from '#lib/i18n/index.svelte';

	let { profiles }: { profiles: ProfileSummary[] } = $props();
	const consents = new Loader<OAuthConsentOut[]>();
	const load = () => consents.run(async () => (await listConsents()).items);
	$effect(() => {
		load();
	});
	let busy = $state(false);
	let error = $state<string | null>(null);
	let confirming = $state<string | null>(null);

	async function revoke(id: string) {
		busy = true;
		error = null;
		try {
			await revokeConsent(id);
			confirming = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
			load();
		}
	}
	const live = $derived((consents.data ?? []).filter((c) => !c.revoked_at && new Date(c.expires_at) > new Date()));
	const name = (id: string) => profiles.find((p) => p.id === id)?.display_name ?? id.slice(0, 8);
	const lastUse = (c: OAuthConsentOut) =>
		c.last_used_at ?? (c.tokens ?? []).map((x) => x.last_used_at).filter((x): x is string => !!x).sort().at(-1) ?? null;
	const host = (u: string) => {
		try {
			return new URL(u).host;
		} catch {
			return u;
		}
	};
</script>

<section class="card" aria-labelledby="ca-h">
	<h3 id="ca-h">{t('fu_ca')}<span class="r">{t('fu_ca_r')}</span></h3>
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	{#if consents.error}
		<NotReady error={consents.error} onretry={load} />
	{:else if consents.loading && !consents.data}
		<div class="empty">{t('loading')}</div>
	{:else}
		<div style="overflow-x:auto">
			<table class="t">
				<tbody>
					{#each live as c (c.id)}
						<tr>
							<td>
								<b>{plain(c.client_name) || host(c.client_id)}</b>
								<div class="dim mono" style="font-size:10.5px;overflow-wrap:anywhere">{c.client_id}</div>
							</td>
							<td>
								<div>{c.profile_ids.map(name).join(', ')}</div>
								<div class="scopes">{#each c.scopes as s (s)}<span class="chip mono">{s}</span>{/each}</div>
							</td>
							<td class="muted" style="white-space:nowrap">
								<div>{t('i2_t_last')}: {ago(lastUse(c))}</div>
								<div class="dim">{tf('fu_ca_until', { date: day(c.expires_at) })}</div>
							</td>
							<td style="text-align:right;white-space:nowrap">
								{#if confirming === c.id}
									<button class="btn sm no" disabled={busy} onclick={() => revoke(c.id)}>{t('fu_ca_revoke_yes')}</button>
									<button class="btn sm" onclick={() => (confirming = null)}>{t('i2_cancel')}</button>
								{:else}
									<button class="btn sm no" onclick={() => (confirming = c.id)}>{t('fu_ca_revoke')}</button>
								{/if}
							</td>
						</tr>
					{:else}
						<tr><td><div class="empty">{t('fu_ca_none')}</div></td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<div class="note" style="margin-top:6px">{t('fu_ca_note')}</div>
</section>

<style>
	.scopes {
		display: flex;
		flex-wrap: wrap;
		gap: 3px;
		margin-top: 3px;
	}
</style>
