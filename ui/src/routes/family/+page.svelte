<script lang="ts">
	// Family (mockup screen #family): the household's profiles, the profile switcher and the grant matrix.
	// Profiles come from GET /me; the full matrix from GET /household (lane G). Pedigree and hints are
	// v0.3, break-glass v1.0: their cards say so.
	import '#lib/components/pages2/pages2.css';
	import { addRelative, getHousehold, getMe, inviteMember, putGrant, type RelativeIn } from '#lib/api';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import type { Household, ProfileSummary, Role, Scope } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, cap, errRaw, errText, tf, tp } from '#lib/components/pages1/util.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { copyText } from '#lib/logic/inbox';
	import { foreignOrigin, matrixHeads, matrixRowLabel, nameTaken, todayIso } from '#lib/logic/people';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import Pedigree from '#lib/components/Pedigree.svelte';
	import FamilyConsent from '#lib/components/FamilyConsent.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { roleCell } from '#lib/logic/grants';
	import { backSummary, giveBackGrant, lapsed, localDay, type FormerAccess } from '#lib/logic/transfer';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import { AVATAR_COLORS, app, currentProfile, setProfile } from '#lib/state/app.svelte';

	const me = $derived(app.me);
	const profiles = $derived(me?.profiles ?? []);
	const onScreen = $derived(currentProfile());
	const hh = new Loader<Household>();

	function load() {
		if (me) hh.run(getHousehold);
	}
	$effect(load);

	const REL = ['self', 'partner', 'child', 'parent'];
	// `relationship` is relative to the household's founder, so 'self' on someone else's profile is not "you".
	const rel = (p: ProfileSummary) =>
		t('h1_rel_' + (p.is_self ? 'self' : p.relationship !== 'self' && REL.includes(p.relationship ?? '') ? p.relationship : 'other'));
	const first = (name: string) => name.split(' ')[0];
	const color = (slug: string) => AVATAR_COLORS[Math.max(0, profiles.findIndex((p) => p.slug === slug)) % AVATAR_COLORS.length];
	// Matrix columns: the household's profiles when the service answers, else the ones I can see.
	const cols = $derived(hh.data?.profiles ?? profiles);
	// Column heads: first names, the full name (plus the short name) when two first names collide.
	const heads = $derived(matrixHeads(cols));
	const today = todayIso();
	// Add a person without a login (a child, a parent), or invite someone who gets their own login.
	let form = $state<'' | 'relative' | 'invite'>('');
	let busy = $state(false);
	// The error shows in the form that failed: 'relative' or 'invite'.
	let error = $state<{ where: string; text: string; raw: string } | null>(null);
	let nr = $state({ display_name: '', relationship: 'child', role: 'guardian', birth_date: '', sex_at_birth: '', country: 'ES' });
	let inv = $state({ display_name: '', locale: i18n.lang as 'es' | 'en', claim_profile: '' });
	let invited = $state<{ name: string; url: string; code: string; days: number } | null>(null);
	// null: not tried yet; false: the browser refused the clipboard and the link is selected instead.
	let copied = $state<boolean | null>(null);
	let urlEl: HTMLElement | undefined = $state();
	const unclaimed = $derived((hh.data?.profiles ?? []).filter((p) => p.can_manage && p.claimed === false));
	const isOwner = $derived(profiles.some((p) => p.role === 'owner'));

	async function act(where: string, fn: () => Promise<void>) {
		busy = true;
		error = null;
		try {
			await withStepUp(fn);
		} catch (e) {
			error = { where, text: errText(e), raw: errRaw(e) };
		} finally {
			busy = false;
		}
	}
	function saveRelative(e: SubmitEvent) {
		e.preventDefault();
		act('relative', async () => {
			const body: RelativeIn = {
				display_name: nr.display_name.trim(),
				role: nr.role as RelativeIn['role'],
				relationship: nr.relationship as RelativeIn['relationship'],
				birth_date: nr.birth_date || null,
				sex_at_birth: (nr.sex_at_birth || null) as RelativeIn['sex_at_birth'],
				country: nr.country.trim().toUpperCase() || null
			};
			const made = await addRelative(body);
			app.me = await getMe();
			setProfile(made.slug);
			load();
			form = '';
			nr = { ...nr, display_name: '', birth_date: '', sex_at_birth: '' };
		});
	}
	function sendInvite(e: SubmitEvent) {
		e.preventDefault();
		act('invite', async () => {
			const name = inv.display_name.trim();
			const r = await inviteMember({ display_name: name, locale: inv.locale, claim_profile: inv.claim_profile || null });
			invited = { name, url: r.url, code: r.code, days: r.expires_in_days };
			copied = null;
			load();
			form = '';
			inv = { ...inv, display_name: '', claim_profile: '' };
		});
	}
	async function copy(text: string) {
		// No clipboard on a plain-HTTP LAN address: only a write that succeeded says "Copied"; otherwise
		// the link is selected and the note says to copy it by hand.
		copied = await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, text, urlEl);
	}
	const otherOrigin = $derived(invited ? foreignOrigin(invited.url, typeof location === 'undefined' ? '' : location.origin) : null);
	const sameName = $derived(nameTaken(nr.display_name, hh.data?.profiles ?? profiles));
	const closeForm = () => {
		form = '';
		error = null;
	};
	const canInvite = $derived(isOwner || unclaimed.length > 0);
	const hasUnknown = $derived(!!hh.data && hh.data.members.some((m) => cols.some((c) => roleCell(hh.data!.grants, m, c.slug, manages(c.slug)) === '?')));
	const manages = (slug: string) => !!hh.data?.profiles.find((p) => p.slug === slug)?.can_manage;

	// Ownership transfer at majority: a guardian sees who may take over their profile now; the person
	// who took over sees who had access before and can give it back.
	type HhProfile = Household['profiles'][number];
	const takeOver = $derived(
		(hh.data?.transfer_pending ?? [])
			.map((x) => ({ p: hh.data?.profiles.find((p) => p.slug === x.profile), invited: x.invited }))
			.filter((x): x is { p: HhProfile; invited: boolean } => !!x.p)
	);
	const mine = $derived(profiles.find((p) => p.is_self && p.role === 'owner' && p.relationship === 'child'));
	const former = $derived(mine ? (hh.data?.former_access ?? []).filter((f) => f.profile === mine.slug) : []);
	// The person whose "give access back" waits for the second click.
	let backing = $state('');
	let gaveBack = $state('');
	const memberName = (id: string) => hh.data?.members.find((m) => m.user_id === id)?.display_name ?? t('xfer_someone');
	function backWhat(f: FormerAccess): string {
		const s = backSummary(f.categories);
		const list = s.keys.length ? tf('xfer_what', { list: s.keys.map((k) => t(k)).join('; ') }) : t('xfer_what_role');
		return s.sensitive ? `${list} ${t('xfer_what_sens')}` : list;
	}
	function inviteToTakeOver(p: HhProfile) {
		inv = { ...inv, display_name: p.display_name, claim_profile: p.slug };
		form = 'invite';
		error = null;
	}
	function giveBack(f: FormerAccess) {
		const slug = mine?.slug;
		if (!slug || lapsed(f.expires_at)) return;
		act('xfer', async () => {
			const g = giveBackGrant(f);
			await putGrant({ ...g, profile: slug, role: g.role as Role, categories: g.categories as Scope[] });
			backing = '';
			gaveBack = memberName(f.user_id);
			load();
		});
	}
</script>

<div class="ph">
	<div>
		<h1>{t('title_fam')}</h1>
		{#if me}<div class="sub">{tp('h1_fam_sub', cols.length, { hh: me.household.name })}</div>{/if}
	</div>
	<div class="right">
		<PhaseChip phase={t('h1_ph_roles')} /><PhaseChip phase={t('h1_ph_ped')} /><PhaseChip phase={t('h1_ph_bg')} />
		<button class="btn" onclick={() => (form === 'relative' ? closeForm() : ((form = 'relative'), (error = null)))} aria-expanded={form === 'relative'}>{t('fm_add')}</button>
		<button class="btn pri" onclick={() => (form === 'invite' ? closeForm() : ((form = 'invite'), (error = null)))} aria-expanded={form === 'invite'} disabled={!canInvite} aria-describedby={canInvite ? undefined : 'fm-inv-why'}>{t('fm_invite')}</button>
		{#if !canInvite && me}<span class="dim" id="fm-inv-why" style="font-size:11.5px;flex-basis:100%;text-align:right">{t('fm_inv_owner')}</span>{/if}
	</div>
</div>

{#if form === 'relative'}
	<form class="card p2form" style="margin-bottom:12px" onsubmit={saveRelative} aria-labelledby="fm-rel-h" use:focusOnOpen={{ oncancel: closeForm }}>
		<h3 id="fm-rel-h">{t('fm_add')}<span class="r">{t('fm_add_d')}</span></h3>
		<div class="p2row">
			<label style="flex:1;min-width:180px">{t('fm_name')}<input class="p2in" bind:value={nr.display_name} required maxlength="80" aria-describedby={sameName ? 'fm-same' : undefined} /></label>
			<label>{t('fm_rel')}
				<select class="p2in" bind:value={nr.relationship}>
					{#each ['child', 'parent', 'partner', 'other'] as r (r)}<option value={r}>{cap(t('h1_rel_' + r))}</option>{/each}
				</select>
			</label>
			<label>{t('fm_your_role')}
				<select class="p2in" bind:value={nr.role}>
					<option value="guardian">{t('r_guardian')}</option>
					<option value="caregiver">{t('r_caregiver')}</option>
				</select>
			</label>
		</div>
		{#if sameName}<div class="note" id="fm-same" style="color:var(--watch)">{t('fm_same_name')}</div>{/if}
		<div class="p2row">
			<label>{t('i2_birth')}<input class="p2in" type="date" min="1900-01-01" max={today} bind:value={nr.birth_date} /></label>
			<label>{t('i2_sex')}
				<select class="p2in" bind:value={nr.sex_at_birth}>
					<option value="">—</option><option value="female">{t('i2_sex_f')}</option><option value="male">{t('i2_sex_m')}</option><option value="unknown">{t('i2_sex_u')}</option>
				</select>
			</label>
			<label>{t('i2_country')}<input class="p2in" bind:value={nr.country} maxlength="2" pattern={'[A-Za-z]{2}'} title={t('i2_country_t')} style="width:56px" /></label>
		</div>
		<div class="note">{t('fm_role_d')}</div>
		{#if error?.where === 'relative'}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
		<div class="p2row"><button class="btn pri" type="submit" disabled={busy}>{t('fm_add_save')}</button><button class="btn" type="button" onclick={closeForm}>{t('i2_cancel')}</button></div>
	</form>
{:else if form === 'invite'}
	<form class="card p2form" style="margin-bottom:12px" onsubmit={sendInvite} aria-labelledby="fm-inv-h" use:focusOnOpen={{ oncancel: closeForm }}>
		<h3 id="fm-inv-h">{t('fm_invite')}<span class="r">{t('fm_inv_d')}</span></h3>
		<div class="p2row">
			<label style="flex:1;min-width:180px">{t('fm_name')}<input class="p2in" bind:value={inv.display_name} required maxlength="80" /></label>
			<label>{t('fm_lang')}
				<select class="p2in" bind:value={inv.locale}><option value="es">Español</option><option value="en">English</option></select>
			</label>
			<label>{t('fm_claim')}
				<select class="p2in" bind:value={inv.claim_profile}>
					{#if isOwner}<option value="">{t('fm_claim_none')}</option>{/if}
					{#each unclaimed as p (p.slug)}<option value={p.slug}>{p.display_name}</option>{/each}
				</select>
			</label>
		</div>
		<div class="note">{t('fm_claim_d')}</div>
		{#if error?.where === 'invite'}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
		<div class="p2row"><button class="btn pri" type="submit" disabled={busy}>{t('fm_inv_make')}</button><button class="btn" type="button" onclick={closeForm}>{t('i2_cancel')}</button></div>
	</form>
{/if}

{#if invited}
	<section class="card p2form" style="margin-bottom:12px" aria-live="polite">
		<h3>{tf('fm_inv_done', { name: invited.name })}<span class="r">{tf('fm_inv_exp', { n: invited.days })}</span></h3>
		<div class="p2secret" data-testid="invite-url" bind:this={urlEl}>{invited.url}</div>
		{#if otherOrigin}<div class="note" style="color:var(--watch)">{tf('fm_inv_origin', { origin: otherOrigin })}</div>{/if}
		<div class="muted" style="font-size:12px">{t('fm_inv_code')} <span class="mono" data-testid="invite-code">{invited.code}</span></div>
		<div class="note" style="color:var(--watch)">{t('fm_inv_once')}</div>
		{#if copied === false}<div class="note" role="alert" style="color:var(--watch)">{t('fm_copy_hand')}</div>{/if}
		<div class="p2row"><button class="btn" onclick={() => invited && copy(invited.url)}>{copied ? t('fm_copied') : t('fm_copy')}</button><button class="btn" onclick={() => (invited = null)}>{t('fm_inv_close')}</button></div>
	</section>
{/if}

{#each takeOver as { p, invited } (p.slug)}
	<section class="card xfer" aria-labelledby="xfer-{p.slug}">
		<h3 id="xfer-{p.slug}">{tf('xfer_title', { name: first(p.display_name), age: p.age ?? '' })}</h3>
		<div class="note">{tf('xfer_d', { name: first(p.display_name) })}</div>
		{#if invited}
			<div class="p2row muted" role="status">{tf('xfer_invited', { name: first(p.display_name) })}</div>
		{:else}
			<div class="p2row"><button class="btn pri" onclick={() => inviteToTakeOver(p)}>{tf('xfer_invite', { name: first(p.display_name) })}</button></div>
		{/if}
	</section>
{/each}
{#if mine && former.length}
	<section class="card xfer" aria-labelledby="xfer-had">
		<h3 id="xfer-had">{t('xfer_had')}<span class="r">{t('xfer_had_r')}</span></h3>
		<div class="note">{t('xfer_had_d')}</div>
		<ul class="xfer-list">
			{#each former as f (f.user_id)}
					{@const gone = lapsed(f.expires_at)}
				<li>
					<span class="who"
						><b>{memberName(f.user_id)}</b> <span class="muted">{t('r_' + f.role).toLowerCase()} · {tf('xfer_until', { date: localDay(f.ended_at) })}</span>
						{#if !gone}<span class="what muted">{backWhat(f)}</span>{/if}</span
					>
					{#if gone && f.expires_at}
						<span class="muted">{tf('xfer_lapsed', { date: localDay(f.expires_at) })}</span>
					{:else if backing === f.user_id}
						<ConfirmRow
							text={tf('xfer_back_q', { name: memberName(f.user_id) })}
							confirmLabel={t('xfer_back_yes')}
							{busy}
							onconfirm={() => giveBack(f)}
							oncancel={() => (backing = '')}
						/>
					{:else}
						<button class="btn" disabled={busy} onclick={() => (backing = f.user_id)}>{t('xfer_back')}</button>
					{/if}
				</li>
			{/each}
		</ul>
		{#if error?.where === 'xfer'}<div class="p2err" role="alert" title={error.raw}>{error.text}</div>{/if}
		{#if !mine.genome_sensitive_ui}<div class="note">{t('xfer_genome')}</div>{/if}
	</section>
{/if}
{#if gaveBack}<div class="note xfer-done" role="status">{tf('xfer_back_done', { name: gaveBack })}</div>{/if}

<div class="mem">
	{#each profiles as p (p.id)}
		{@const on = p.slug === onScreen?.slug}
		<button
			class="mcard"
			style="text-align:left;{on ? 'border-color:var(--accent);background:var(--raised)' : ''}"
			aria-pressed={on}
			title={tf('h1_show', { name: p.display_name })}
			onclick={() => setProfile(p.slug)}
		>
			<div class="av" style="background:{color(p.slug)};width:34px;height:34px">{p.initials}</div>
			<div style="min-width:0">
				<div style="font-weight:600">{p.display_name}{#if on}<span class="sr-only"> · {t('h1_on_screen')}</span>{/if}</div>
				<div class="muted" style="font-size:11.5px">{rel(p)}{p.age != null ? ', ' + p.age : ''} · {tf('h1_your_role', { r: t('r_' + p.role).toLowerCase() })}</div>
				{#if p.is_minor}<div class="dim" style="font-size:11px">{t('h1_minor')}</div>{/if}
			</div>
		</button>
	{/each}
</div>
{#if me && !profiles.length}<div class="card" style="margin-bottom:12px"><div class="empty">{t('h1_noprofile')}</div></div>{/if}

<div class="fam">
	<div class="grid" style="align-content:start">
		<!-- keyed: a drawer left open for one profile must never save under the next one -->
		{#if onScreen}{#key onScreen.slug}<Pedigree profile={onScreen} {profiles} />{/key}{/if}
	</div>
	<div class="grid" style="align-content:start">
		<div class="card">
			<h3>{t('h1_grants')}<span class="r">{t('h1_grants_r')}</span></h3>
			{#if hh.loading && !hh.data}
				<div class="empty">{t('loading')}</div>
			{:else}
				<div class="tscroll">
				<table class="t gm">
					<thead>
						<tr><th>{t('h1_viewer_profile')}</th>{#each heads as c (c.slug)}<th title={c.slug}>{c.label}{#if c.dup}<div class="dim mono" style="font-size:10px;text-transform:none;letter-spacing:0">{c.slug}</div>{/if}</th>{/each}</tr>
					</thead>
					<tbody>
						{#if hh.data}
							{#each hh.data.members as m (m.user_id)}
								<tr class:pending={m.invite_pending}>
									<td>{matrixRowLabel(m, hh.data.members, cols, t('h1_you'))}{#if m.invite_pending}<span class="dim">{' · ' + t('h1_invite_pending')}</span>{/if}</td>
									{#each cols as c (c.slug)}
										{@const r = roleCell(hh.data.grants, m, c.slug, manages(c.slug))}
										<td>{#if r === '?'}<span class="dim" title={t('h1_grant_unknown')}>?</span>{:else if r}{t('r_' + r).toLowerCase()}{:else}<span class="dim">—</span>{/if}</td>
									{/each}
								</tr>
							{/each}
						{:else if me}
							<tr>
								<td>{first(me.user.display_name)} <span class="dim">({t('h1_you')})</span></td>
								{#each cols as c (c.slug)}<td>{t('r_' + c.role).toLowerCase()}</td>{/each}
							</tr>
						{/if}
					</tbody>
				</table>
				</div>
				{#if hasUnknown}<div class="note" style="margin-top:6px">{t('h1_grant_q')}</div>{/if}
				{#if hh.error}
					<div class="note" style="margin-top:6px">{t('h1_grants_me')}</div>
					<NotReady error={hh.error} onretry={load} />
				{/if}
			{/if}
			<div class="note" style="margin-top:6px">{t('h1_grants_note')}</div>
		</div>
		{#if onScreen && hh.data}{#key onScreen.slug}<FamilyConsent profile={onScreen} manages={manages(onScreen.slug)} />{/key}{/if}
		<div class="card">
			<h3>{t('h1_bg')}<span class="r">{t('h1_bg_r')}</span></h3>
			<div class="empty"><PhaseChip phase="v1.0" /> {t('h1_bg_d')}</div>
		</div>
	</div>
</div>

<StepUpDialog />

<style>
	/* The matrix scrolls inside its card; the card never grows past its column. */
	:global(.fam > *),
	:global(.fam > * > *) {
		min-width: 0;
	}
	tr.pending td {
		color: var(--muted);
	}
	.xfer {
		margin-bottom: 12px;
		border-color: var(--accent);
	}
	.xfer .p2row {
		margin-top: 8px;
	}
	.xfer-list {
		list-style: none;
		margin: 8px 0;
		padding: 0;
	}
	.xfer-list li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 6px 0;
		border-top: 1px solid var(--line);
	}
	.xfer-list .what {
		display: block;
		font-size: 12px;
	}
	.xfer-done {
		margin: -4px 0 12px;
	}
	/* Phone: each field of the two forms takes a whole line, and a long option never sets the width. */
	@media (max-width: 760px) {
		.p2form > * {
			min-width: 0;
		}
		.p2form .p2row > label {
			flex: 1 1 100%;
			min-width: 0 !important;
		}
		.p2form select.p2in {
			width: 100%;
			max-width: 100%;
			min-width: 0;
		}
		.p2form h3 .r {
			white-space: normal;
		}
	}
</style>
