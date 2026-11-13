<script lang="ts">
	// Settings: your passkeys and recovery codes, and for each profile you manage its locks, the two
	// sensitive-genome switches, and who has access (grants). Changes that widen access need step-up.
	import { tick } from 'svelte';
	import '#lib/components/pages2/pages2.css';
	import {
		addPasskeyBegin,
		addPasskeyFinish,
		deleteGrant,
		deleteProfile,
		exportProfile,
		getMe,
		getAuthState,
		getHousehold,
		listPasskeys,
		logout,
		newRecoveryCodes,
		putGrant,
		putLocks,
		putSensitive,
		removePasskey
	} from '#lib/api';
	import type { AuthState, Household, PasskeyOut, Role, Scope } from '#lib/api/types';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, scopeText, tf } from '#lib/components/pages1/util.svelte';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import Switch from '#lib/components/pages2/Switch.svelte';
	import ConnectedApps from '#lib/components/inbox/ConnectedApps.svelte';
	import DueAnswers from '#lib/components/DueAnswers.svelte';
	import { ago, day } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { createPasskey } from '#lib/components/pages2/webauthn';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { t } from '#lib/i18n/index.svelte';
	import { lastSignIn } from '#lib/logic/people';
	import { app, setProfile } from '#lib/state/app.svelte';

	const LOCKABLE: Scope[] = ['read:labs', 'read:vitals', 'read:wearables', 'read:records', 'read:genome', 'read:genome_raw', 'read:sensitive', 'read:documents_text'];
	const ROLES: Role[] = ['viewer', 'caregiver', 'guardian'];

	const auth = new Loader<AuthState>();
	const keys = new Loader<PasskeyOut[]>();
	const hh = new Loader<Household>();
	let codes = $state<string[] | null>(null);
	// The error sits next to what failed: 'pk' passkeys, 'rc' recovery codes, 'grants', or a profile slug.
	let error = $state<{ where: string; text: string; raw: string } | null>(null);
	let busy = $state(false);
	// Which two-step confirm is open: 'rc', 'pk:<id>' or 'g:<user>:<profile>'.
	let confirming = $state<string | null>(null);

	const loadAll = () => {
		auth.run(getAuthState);
		keys.run(async () => (await listPasskeys()).items);
		hh.run(getHousehold);
	};
	$effect(loadAll);

	async function act(where: string, fn: () => Promise<unknown>, reload = true) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
			confirming = null;
			if (reload) loadAll();
		} catch (e) {
			error = { where, text: errText(e), raw: errRaw(e) };
			// The grant select shows what was picked, not g.role; reload so it matches the server again.
			if (reload) loadAll();
		} finally {
			busy = false;
		}
	}
	const addKey = () => act('pk', async () => addPasskeyFinish(await createPasskey(await addPasskeyBegin()), navigator.platform || undefined));
	const noCodes = $derived(!codes && auth.data?.user?.recovery_codes_left === 0);
	// The new codes are what is left now; focus moves to them, since the button that had it is replaced.
	let codesEl: HTMLElement | undefined = $state();
	const codesLeft = $derived(codes ? codes.length : auth.data?.user?.recovery_codes_left);
	const regen = () =>
		act('rc', async () => {
			codes = (await newRecoveryCodes()).recovery_codes;
			await tick();
			codesEl?.focus();
		}, false);
	async function signOut() {
		await logout().catch(() => null);
		location.href = '/';
	}
	const managed = $derived((hh.data?.profiles ?? []).filter((p) => p.can_manage));
	const profileName = (slug: string) => hh.data?.profiles.find((p) => p.slug === slug)?.display_name ?? slug;
	const mine = $derived(hh.data?.members.find((m) => m.is_me));
	// Removing this passkey leaves only the recovery codes when there is no password and no single sign-on.
	const lastKey = $derived(lastSignIn(keys.data?.length ?? 0, !!mine?.logins?.password, !!auth.data?.user?.oidc_linked));
	const member = (id: string) => hh.data?.members.find((m) => m.user_id === id)?.display_name ?? id.slice(0, 8);
	const me = $derived(hh.data?.members.find((m) => m.is_me)?.user_id);
	const editableGrants = $derived((hh.data?.grants ?? []).filter((g) => g.role !== 'owner' && g.user_id !== me && managed.some((p) => p.slug === g.profile)));
	// A new grant for a member who has none on that profile yet.
	let ng = $state({ user_id: '', profile: '', role: 'viewer' as Role });
	const others = $derived((hh.data?.members ?? []).filter((m) => !m.is_me));
	const hasGrant = (u: string, slug: string) => (hh.data?.grants ?? []).some((g) => g.user_id === u && g.profile === slug);
	const freeProfiles = $derived(managed.filter((p) => ng.user_id && !hasGrant(ng.user_id, p.slug)));
	function addGrant(e: SubmitEvent) {
		e.preventDefault();
		const body = { ...ng };
		act('grants', async () => {
			await putGrant(body);
			ng = { user_id: '', profile: '', role: 'viewer' };
		});
	}
	// Full export, saved through a temporary link so the browser downloads the zip.
	const download = (slug: string) =>
		act(slug, async () => {
			const { blob, filename } = await exportProfile(slug);
			const a = document.createElement('a');
			a.href = URL.createObjectURL(blob);
			a.download = filename;
			document.body.appendChild(a);
			a.click();
			a.remove();
			setTimeout(() => URL.revokeObjectURL(a.href), 1000);
		}, false);
	// Delete a profile: type its short name to confirm, then step-up.
	let deleting = $state<string | null>(null);
	let confirmSlug = $state('');
	function removeProfile(e: SubmitEvent, slug: string) {
		e.preventDefault();
		act(slug, async () => {
			await deleteProfile(slug);
			deleting = null;
			confirmSlug = '';
			app.me = await getMe();
			if (app.profile === slug) {
				const own = app.me.profiles.find((p) => p.is_self) ?? app.me.profiles[0];
				if (own) setProfile(own.slug);
			}
		});
	}
	function toggleLock(slug: string, locks: Scope[], s: Scope, on: boolean) {
		const next = on ? [...locks, s] : locks.filter((x) => x !== s);
		act(slug, () => putLocks(slug, next));
	}
</script>

<div class="ph">
	<div>
		<h1>{t('i2_settings')}</h1>
		<div class="sub">{t('i2_set_sub')}</div>
	</div>
	<div class="right"><a class="btn" href="/mcp-access">{t('nav_mcp')}</a><button class="btn" onclick={signOut}>{t('i2_logout')}</button></div>
</div>

{#snippet fail(where: string)}
	{#if error?.where === where}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
{/snippet}

<div class="cols">
	<div class="grid" style="align-content:start">
		<section class="card">
			<h3>{t('i2_passkeys')}</h3>
			{#if keys.error}
				<NotReady error={keys.error} onretry={loadAll} />
			{:else}
				<table class="t stack">
					<tbody>
						{#each keys.data ?? [] as k (k.id)}
							<tr>
								<td><b>{k.label ?? t('i2_pk_unnamed')}</b><div class="dim mono" style="font-size:10.5px">{k.id.slice(0, 12)}…</div></td>
								<td class="muted">{t('i2_added')} <span class="num" style="white-space:nowrap">{day(k.created_at)}</span></td>
								<td class="muted">{t('i2_t_last')}: <span class="num" style="white-space:nowrap">{ago(k.last_used_at)}</span></td>
								<td class="acts-cell" style="text-align:right">
									<button class="btn sm no" disabled={busy} aria-expanded={confirming === 'pk:' + k.id} onclick={() => (confirming = 'pk:' + k.id)}>{t('i2_remove')}</button>
								</td>
							</tr>
							{#if confirming === 'pk:' + k.id}
								<tr>
									<td colspan="4">
										<ConfirmRow
											text={lastKey ? t('i2_pk_remove_last') : tf('i2_pk_remove_d', { label: k.label ?? t('i2_pk_unnamed') })}
											confirmLabel={t('i2_remove')}
											{busy}
											onconfirm={() => act('pk', () => removePasskey(k.id))}
											oncancel={() => (confirming = null)}
										/>
									</td>
								</tr>
							{/if}
						{:else}
							<tr><td colspan="4" style="font-weight:400">{#if keys.loading || !keys.data}<div class="empty">{t('loading')}</div>{:else}<div class="empty">{t('i2_pk_none')}</div>{/if}</td></tr>
						{/each}
					</tbody>
				</table>
			{/if}
			<div class="p2row" style="margin-top:8px">
				<button class="btn" onclick={addKey} disabled={busy || !auth.data?.passkeys_possible} aria-describedby="pk-why">{t('i2_pk_add')}</button>
				<span class="dim" id="pk-why" style="font-size:11.5px">{auth.data && !auth.data.passkeys_possible ? t('i2_pk_need_https') : t('i2_pk_add_d')}</span>
			</div>
			{@render fail('pk')}
		</section>
		<section class="card">
			<h3>{t('i2_rc')}<span class="r">{codesLeft != null ? tf('i2_rc_left', { n: codesLeft }) : ''}</span></h3>
			<div class="muted" style="font-size:12px">{t('i2_rc_d')}</div>
			{#if codes}
				<div class="p2secret" style="margin-top:8px;columns:2" aria-live="polite" tabindex="-1" bind:this={codesEl}>{#each codes as c (c)}<div>{c}</div>{/each}</div>
				<div class="note" style="margin-top:6px;color:var(--watch)">{t('i2_rc_once')}</div>
			{/if}
			<div class="p2row" style="margin-top:8px">
				<!-- With no code left there is nothing to replace, so the first set needs no warning. -->
				{#if noCodes}
					<button class="btn" onclick={regen} disabled={busy}>{t('i2_rc_create')}</button>
				{:else}
					<button class="btn" onclick={() => (confirming = 'rc')} disabled={busy} aria-expanded={confirming === 'rc'}>{t('i2_rc_new')}</button>
				{/if}
			</div>
			{#if confirming === 'rc' && !noCodes}
				<div style="margin-top:8px"><ConfirmRow text={t('i2_rc_new_d')} confirmLabel={t('i2_rc_new_yes')} {busy} onconfirm={regen} oncancel={() => (confirming = null)} /></div>
			{/if}
			{@render fail('rc')}
		</section>
		<ConnectedApps profiles={app.me?.profiles ?? []} />
		<section class="card" style="padding:8px 6px 2px">
			<h3 style="padding:0 8px">{t('i2_grants')}<span class="r">{t('i2_grants_d')}</span></h3>
			{#if hh.error}
				<NotReady error={hh.error} onretry={loadAll} />
			{:else}
				<div class="tscroll">
				<table class="t stack">
					<thead><tr><th>{t('i2_person')}</th><th>{t('profile')}</th><th>{t('i2_role')}</th><th>{t('i2_t_exp')}</th><th></th></tr></thead>
					<tbody>
						{#each editableGrants as g (g.user_id + g.profile)}
							{@const key = 'g:' + g.user_id + ':' + g.profile}
							<tr>
								<td data-label={t('i2_person')}>{member(g.user_id)}</td>
								<td data-label={t('profile')}>{profileName(g.profile)} <span class="dim mono" style="font-size:10.5px">{g.profile}</span></td>
								<td data-label={t('i2_role')}>
									<select class="p2in" value={g.role} aria-label="{t('i2_role')} {member(g.user_id)} · {profileName(g.profile)}" disabled={busy} onchange={(e) => act('grants', () => putGrant({ user_id: g.user_id, profile: g.profile, role: (e.currentTarget as HTMLSelectElement).value as Role }))}>
										{#each ROLES as r (r)}<option value={r}>{t('r_' + r)}</option>{/each}
									</select>
								</td>
								<td class="muted" data-label={t('i2_t_exp')}><span class="num" style="white-space:nowrap">{g.expires_at ? day(g.expires_at) : t('i2_never')}</span></td>
								<td class="acts-cell" style="text-align:right">
									<button class="btn sm no" disabled={busy} aria-expanded={confirming === key} aria-label="{t('i2_revoke')} {member(g.user_id)} · {profileName(g.profile)}" onclick={() => (confirming = key)}>{t('i2_revoke')}</button>
								</td>
							</tr>
							{#if confirming === key}
								<tr>
									<td colspan="5">
										<ConfirmRow
											text={tf('i2_grant_revoke_d', { person: member(g.user_id), profile: profileName(g.profile) })}
											confirmLabel={t('fu_ca_revoke_yes')}
											{busy}
											onconfirm={() => act('grants', () => deleteGrant(g.user_id, g.profile))}
											oncancel={() => (confirming = null)}
										/>
									</td>
								</tr>
							{/if}
						{:else}
							<tr><td colspan="5" style="font-weight:400">{#if !hh.data}<div class="empty">{t('loading')}</div>{:else}<div class="empty">{t('i2_grants_none')}</div>{/if}</td></tr>
						{/each}
					</tbody>
				</table>
				</div>
				{#if managed.length && others.length}
					<form class="p2row" style="padding:8px" onsubmit={addGrant} aria-label={t('st_grant_new')}>
						<b style="font-size:12px">{t('st_grant_new')}</b>
						<select class="p2in" bind:value={ng.user_id} required aria-label={t('i2_person')} onchange={() => (ng.profile = '')}>
							<option value="">{t('i2_person')}…</option>
							{#each others as m (m.user_id)}<option value={m.user_id}>{m.display_name}</option>{/each}
						</select>
						<select class="p2in" bind:value={ng.profile} required aria-label={t('profile')} disabled={!ng.user_id}>
							<option value="">{t('profile')}…</option>
							{#each freeProfiles as p (p.slug)}<option value={p.slug}>{p.display_name}</option>{/each}
						</select>
						<select class="p2in" bind:value={ng.role} aria-label={t('i2_role')}>
							{#each ROLES as r (r)}<option value={r}>{t('r_' + r)}</option>{/each}
						</select>
						<button class="btn sm pri" type="submit" disabled={busy || !ng.user_id || !ng.profile}>{t('st_grant_add')}</button>
					</form>
				{/if}
				<div style="padding:0 8px">{@render fail('grants')}</div>
				<div class="note" style="padding:6px 8px">{t('i2_grants_note')} <a href="/family">{t('nav_fam')}</a></div>
			{/if}
		</section>
	</div>

	<div class="grid" style="align-content:start">
		{#each managed as p (p.id)}
			<section class="card">
				<h3>{p.display_name}<span class="r">{t('r_' + p.role)}{p.is_minor ? ' · ' + t('i2_minor') : ''}</span></h3>
				{@render fail(p.slug)}
				<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin-bottom:2px">{t('i2_sensitive')}</div>
				<div class="set">
					<Switch on={p.genome_sensitive_ui} label="{t('i2_sens_ui')} · {p.display_name}" disabled={busy || (p.is_minor && !p.genome_sensitive_ui)} onchange={(on) => act(p.slug, () => putSensitive(p.slug, { ui: on }))} />
					<div style="flex:1;min-width:0"><div>{t('i2_sens_ui')}</div><div class="dim" style="font-size:11px">{t('i2_sens_ui_d')}</div></div>
				</div>
				<div class="set">
					<Switch on={!!p.genome_sensitive_agents} label="{t('i2_sens_ag')} · {p.display_name}" disabled={busy || (p.is_minor && !p.genome_sensitive_agents)} onchange={(on) => act(p.slug, () => putSensitive(p.slug, { agents: on }))} />
					<div style="flex:1;min-width:0"><div>{t('i2_sens_ag')}</div><div class="dim" style="font-size:11px">{t('i2_sens_ag_d')}</div></div>
				</div>
				{#if p.is_minor}<div class="note">{t('i2_minor_d')}</div>{/if}
				<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin:10px 0 2px">{t('i2_locks')}</div>
				<div class="note" style="margin-bottom:4px">{t('i2_locks_d')}</div>
				{#each LOCKABLE as s (s)}
					{@const locked = (p.locks ?? []).includes(s)}
					<div class="set">
						<Switch on={locked} label="{t('i2_lock')} {scopeText(s)} · {p.display_name}" words={[t('i2_lock_on'), t('i2_lock_off')]} disabled={busy} onchange={(on) => toggleLock(p.slug, p.locks ?? [], s, on)} />
						<div style="flex:1;min-width:0"><div>{scopeText(s)}</div><div class="dim mono" style="font-size:10.5px">{s}</div></div>
					</div>
				{/each}
				<DueAnswers profile={p.slug} canWrite bare />
				<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em;margin:10px 0 2px">{t('st_data')}</div>
				<div class="set">
					<div style="flex:1;min-width:0"><div>{t('st_export')}</div><div class="dim" style="font-size:11px">{t('st_export_d')}</div></div>
					<button class="btn sm" disabled={busy} onclick={() => download(p.slug)} aria-label="{t('st_export')} · {p.display_name}">{t('st_export_btn')}</button>
				</div>
				<div class="set">
					<div style="flex:1;min-width:0"><div>{t('st_delete')}</div><div class="dim" style="font-size:11px">{t('st_delete_d')}</div></div>
					<button class="btn sm no" disabled={busy} onclick={() => ((deleting = deleting === p.slug ? null : p.slug), (confirmSlug = ''))} aria-expanded={deleting === p.slug}>{t('st_delete_btn')}</button>
				</div>
				{#if deleting === p.slug}
					<form class="delrow" onsubmit={(e) => removeProfile(e, p.slug)} use:focusOnOpen={{ oncancel: () => (deleting = null) }}>
						{#if p.is_self}<div class="note" style="color:var(--watch)">{t('st_delete_self')}</div>{/if}
						<label class="muted" style="font-size:12px" for="del-{p.slug}">{t('st_delete_type')} <b class="mono">{p.slug}</b></label>
						<div class="p2row">
							<input id="del-{p.slug}" class="p2in mono" style="flex:1;min-width:160px" bind:value={confirmSlug} required autocomplete="off" />
							<button class="btn sm" type="button" onclick={() => (deleting = null)}>{t('i2_cancel')}</button>
							<button class="btn sm no" type="submit" disabled={busy || confirmSlug !== p.slug}>{t('st_delete_btn')}</button>
						</div>
					</form>
				{/if}
			</section>
		{:else}
			{#if hh.data}<section class="card"><div class="empty">{t('i2_manage_none')}</div></section>{/if}
		{/each}
	</div>
</div>

<StepUpDialog />

<style>
	.cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 400px;
		gap: 14px;
	}
	.cols > *,
	.cols > * > :global(*) {
		min-width: 0;
	}
	.delrow {
		display: grid;
		gap: 6px;
		margin-top: 6px;
	}
	@media (max-width: 1100px) {
		.cols {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
