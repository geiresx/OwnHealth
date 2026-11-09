<script lang="ts">
	// The allowlist of one mailbox: who may send, into which profile, and the PDF passwords tried on
	// that sender's locked PDFs (only that sender's, never a global list). Adding a sender widens what
	// flows in, so it needs step-up; removing narrows and does not.
	import { addPdfPassword, addSender, deletePdfPassword, deleteSender, listSenders, type SenderRuleOut } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader } from '#lib/components/pages1/util.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { senderPattern } from '#lib/logic/inbox';

	let { sourceId, profiles, onchange }: { sourceId: string; profiles: ProfileSummary[]; onchange?: () => void } = $props();

	const rules = new Loader<SenderRuleOut[]>();
	const load = () => rules.run(async () => (await listSenders(sourceId)).items, sourceId);
	$effect(() => {
		void sourceId;
		load();
	});

	let busy = $state(false);
	let error = $state<string | null>(null);
	let nr = $state({ pattern: '', profile_id: '', trust_unauthenticated: false });
	let pwFor = $state<string | null>(null);
	let pw = $state({ password: '', label: '' });

	async function act(fn: () => Promise<unknown>) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
			load();
			onchange?.();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
	function add(e: SubmitEvent) {
		e.preventDefault();
		const pattern = senderPattern(nr.pattern);
		if (!pattern) {
			error = t('fu_err_pattern');
			return;
		}
		const body = { pattern, profile_id: nr.profile_id, trust_unauthenticated: nr.trust_unauthenticated };
		act(async () => {
			await addSender(sourceId, body);
			nr = { pattern: '', profile_id: '', trust_unauthenticated: false };
		});
	}
	function savePw(e: SubmitEvent, ruleId: string) {
		e.preventDefault();
		const body = { password: pw.password, label: pw.label.trim() || null };
		act(async () => {
			await addPdfPassword(ruleId, body);
			pw = { password: '', label: '' };
			pwFor = null;
		});
	}
	const profileName = (id: string) => profiles.find((p) => p.id === id)?.display_name ?? id.slice(0, 8);
</script>

<div class="sr">
	<div class="hd"><b>{t('fu_senders')}</b><span class="dim">{t('fu_senders_d')}</span></div>
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	{#if rules.error}
		<NotReady error={rules.error} onretry={load} />
	{:else}
		<div style="overflow-x:auto">
			<table class="t">
				<tbody>
					{#each rules.data ?? [] as r (r.id)}
						<tr>
							<td class="mono">{r.pattern}</td>
							<td>→ {profileName(r.profile_id)}</td>
							<td>
								{#if r.trust_unauthenticated}<span class="pill p-watch" title={t('fu_unauth_d')}><i aria-hidden="true"></i>{t('fu_unauth')}</span>{:else}<span class="pill p-ok"><i aria-hidden="true"></i>DKIM/SPF</span>{/if}
							</td>
							<td>
								{#each r.pdf_passwords as p (p.id)}
									<span class="chip pw">🔑 {p.label ?? t('fu_pw_unnamed')} <button class="x" aria-label="{t('i2_remove')} {p.label ?? ''}" disabled={busy} onclick={() => act(() => deletePdfPassword(p.id))}>×</button></span>
								{/each}
								<button class="btn sm" onclick={() => (pwFor = pwFor === r.id ? null : r.id)} aria-expanded={pwFor === r.id}>{t('fu_pw_add')}</button>
							</td>
							<td style="text-align:right"><button class="btn sm no" disabled={busy} onclick={() => act(() => deleteSender(r.id))}>{t('i2_remove')}</button></td>
						</tr>
						{#if pwFor === r.id}
							<tr>
								<td colspan="5">
									<form class="p2row" onsubmit={(e) => savePw(e, r.id)}>
										<input class="p2in mono" type="password" bind:value={pw.password} required autocomplete="new-password" placeholder={t('fu_pw')} aria-label={t('fu_pw')} />
										<input class="p2in" bind:value={pw.label} maxlength="60" placeholder={t('fu_pw_label')} aria-label={t('fu_pw_label')} />
										<button class="btn sm pri" type="submit" disabled={busy}>{t('fu_pw_save')}</button>
										<span class="dim" style="font-size:11px">🔒 {t('fu_pw_d')}</span>
									</form>
								</td>
							</tr>
						{/if}
					{:else}
						<tr><td><div class="empty" style="padding:8px">{t('fu_senders_none')}</div></td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<form class="p2row add" onsubmit={add}>
		<input class="p2in mono" style="flex:1;min-width:200px" bind:value={nr.pattern} required placeholder="resultados@labnorte.example" aria-label={t('fu_sender')} />
		<select class="p2in" bind:value={nr.profile_id} required aria-label={t('fu_f_profile')}>
			<option value="" disabled>{t('fu_f_profile_pick')}</option>
			{#each profiles as p (p.id)}<option value={p.id}>{p.display_name}</option>{/each}
		</select>
		<label class="ck" title={t('fu_unauth_d')}><input type="checkbox" bind:checked={nr.trust_unauthenticated} /> {t('fu_unauth')}</label>
		<button class="btn sm pri" type="submit" disabled={busy}>{t('fu_sender_add')}</button>
	</form>
	{#if nr.trust_unauthenticated}<div class="note" style="color:var(--watch)">{t('fu_unauth_d')}</div>{/if}
</div>

<style>
	.sr {
		border-top: 1px solid var(--line);
		margin-top: 8px;
		padding-top: 8px;
		display: grid;
		gap: 6px;
	}
	.hd {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
		font-size: 12px;
	}
	.pw {
		margin-right: 4px;
	}
	.x {
		background: none;
		border: 0;
		padding: 0 0 0 2px;
		color: var(--dim);
	}
	.x:hover {
		color: var(--out);
	}
	.ck {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		font-size: 12px;
	}
	.add {
		margin-top: 2px;
	}
</style>
