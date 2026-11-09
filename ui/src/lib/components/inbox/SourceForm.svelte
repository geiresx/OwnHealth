<script lang="ts">
	// Add or edit one inbox: mailbox (IMAP), watched folder, Paperless-ngx, Health Auto Export, iOS
	// Shortcut, Nightscout. Secrets go to the server once and are sealed there; they never come back,
	// so editing leaves the field empty and an empty field keeps the stored one.
	import { createInbox, updateInbox, type InboxSource, type SourceKind } from '#lib/api/inbox';
	import type { ProfileSummary } from '#lib/api/types';
	import { tf } from '#lib/components/pages1/util.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { ADDABLE, POLL_KINDS, PROFILE_KINDS, RECIPE, RECIPES_URL, SECRET_OF, TLS_MODES, emptyForm, formOf, portForTls, sourceBody, type SourceForm, type TlsMode } from '#lib/logic/inbox';

	let {
		profiles,
		source = null,
		onsaved,
		oncancel
	}: { profiles: ProfileSummary[]; source?: InboxSource | null; onsaved: (s: InboxSource) => void; oncancel: () => void } = $props();

	// Captured once: the form edits its own copy.
	const initial = () => (source ? formOf(source) : emptyForm('email'));
	let f = $state<SourceForm>(initial());
	let error = $state<string | null>(null);
	let busy = $state(false);
	const editing = $derived(source != null);

	function pick(kind: SourceKind) {
		const keep = { label: f.label, profile_id: f.profile_id };
		f = { ...emptyForm(kind), ...keep };
		error = null;
	}

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		const r = sourceBody(f, editing);
		if ('error' in r) {
			error = t(r.error);
			return;
		}
		busy = true;
		error = null;
		try {
			const { kind, ...patch } = r.body;
			const saved = source ? await withStepUp(() => updateInbox(source.id, patch)) : await withStepUp(() => createInbox({ kind, ...patch }));
			onsaved(saved);
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
		} finally {
			busy = false;
		}
	}
	const secretKey = $derived(SECRET_OF[f.kind]);
	const profileRequired = $derived(PROFILE_KINDS.includes(f.kind));
</script>

<form class="card p2form sf" onsubmit={submit} aria-labelledby="sf-h">
	<h3 id="sf-h">{editing ? tf('fu_edit_t', { label: source?.label ?? '' }) : t('fu_add_t')}<span class="r">{t('fu_add_r')}</span></h3>
	{#if !editing}
		<div class="kinds" role="radiogroup" aria-label={t('fu_kind')}>
			{#each ADDABLE as k (k)}
				<button type="button" class="chip" class:on={f.kind === k} role="radio" aria-checked={f.kind === k} onclick={() => pick(k)}>{t('fu_k_' + k)}</button>
			{/each}
		</div>
	{/if}
	<div class="note">
		{t('fu_kd_' + f.kind)}
		{#if RECIPE[f.kind]}<a href={RECIPES_URL + RECIPE[f.kind]} target="_blank" rel="noopener noreferrer">{t('fu_recipe')} ↗</a>{/if}
	</div>

	<div class="two">
		<label>{t('fu_f_label')}<input class="p2in" bind:value={f.label} required maxlength="80" placeholder={t('fu_ph_' + f.kind)} /></label>
		<label
			>{t('fu_f_profile')}
			<select class="p2in" bind:value={f.profile_id} required={profileRequired}>
				{#if !profileRequired}<option value="">{t(f.kind === 'folder' ? 'fu_f_profile_folder' : 'fu_f_profile_rule')}</option>{:else}<option value="" disabled>{t('fu_f_profile_pick')}</option>{/if}
				{#each profiles as p (p.id)}<option value={p.id}>{p.display_name}</option>{/each}
			</select>
		</label>
	</div>

	{#if f.kind === 'email'}
		<div class="two">
			<label>{t('fu_f_host')}<input class="p2in mono" bind:value={f.host} required placeholder="imap.rivera.example" autocomplete="off" /></label>
			<label>{t('fu_f_port')}<input class="p2in num" bind:value={f.port} inputmode="numeric" required style="max-width:100px" /></label>
			<label
				>{t('fu_f_tls')}
				<select
					class="p2in"
					value={f.tls}
					onchange={(e) => {
						const to = e.currentTarget.value as TlsMode;
						f.port = portForTls(f.tls, to, f.port);
						f.tls = to;
					}}
				>
					{#each TLS_MODES as m (m)}<option value={m}>{t('fu_tls_' + m)}</option>{/each}
				</select>
			</label>
		</div>
		<div class="two">
			<label>{t('fu_f_user')}<input class="p2in mono" bind:value={f.username} required autocomplete="off" placeholder="salud@rivera.example" /></label>
			<label>{t('fu_f_folder')}<input class="p2in mono" bind:value={f.folder} placeholder="INBOX" /></label>
		</div>
		<div class="two">
			<label>{t('fu_f_done')}<input class="p2in mono" bind:value={f.done_folder} placeholder="OwnHealth/Done" /></label>
			<label>{t('fu_f_authserv')}<input class="p2in mono" bind:value={f.trusted_authserv_id} placeholder="mx.rivera.example" /></label>
		</div>
		<div class="note">{t('fu_f_authserv_d')}</div>
	{:else if f.kind === 'folder'}
		<label>{t('fu_f_path')}<input class="p2in mono" bind:value={f.path} placeholder="scans" /></label>
		<div class="note mono">OWNHEALTH_INBOX_DIR/{f.path.replace(/^\/+|\/+$/g, '') ? f.path.replace(/^\/+|\/+$/g, '') + '/' : ''}{f.profile_id ? '' : '<profile>/'}… · {t('fu_f_path_d')}</div>
	{:else if f.kind === 'paperless' || f.kind === 'nightscout'}
		<label>{t('fu_f_url')}<input class="p2in mono" type="url" bind:value={f.url} required placeholder={f.kind === 'paperless' ? 'https://paperless.rivera.example' : 'https://ns.rivera.example'} /></label>
		{#if f.kind === 'paperless'}
			<label>{t('fu_f_tag')}<input class="p2in mono" bind:value={f.tag} placeholder="health" style="max-width:200px" /></label>
		{/if}
		<label class="ck"><input type="checkbox" bind:checked={f.allow_private} /> {t('fu_f_lan')}</label>
		<div class="note">{t('fu_f_lan_d')}</div>
	{/if}

	{#if secretKey}
		<label
			>{t('fu_s_' + secretKey)}
			<input class="p2in mono" type="password" bind:value={f.secret} autocomplete="new-password" required={!editing} placeholder={editing && source?.has_secret !== false ? t('fu_f_secret_keep') : ''} />
		</label>
		<div class="note">🔒 {t('fu_f_secret_d')}</div>
	{/if}

	{#if POLL_KINDS.includes(f.kind)}
		<label>{t('fu_f_poll')}<input class="p2in num" type="number" min="1" max="1440" bind:value={f.poll_minutes} style="max-width:100px" /></label>
	{:else}
		<div class="note">{t('fu_f_push_d')}</div>
	{/if}

	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	<div class="p2row" style="justify-content:flex-end">
		<button type="button" class="btn" onclick={oncancel}>{t('i2_cancel')}</button>
		<button type="submit" class="btn pri" disabled={busy}>{editing ? t('fu_save') : t('fu_add')}</button>
	</div>
</form>

<style>
	.sf {
		max-width: 760px;
		margin-bottom: 12px;
	}
	.kinds {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.kinds .chip {
		padding: 4px 10px;
		font-size: 12px;
	}
	.two {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
		gap: 8px;
	}
	.ck {
		display: inline-flex !important;
		align-items: center;
		gap: 6px;
		font-size: 12.5px !important;
		color: var(--text) !important;
	}
</style>
