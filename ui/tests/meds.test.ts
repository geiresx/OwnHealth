// The Medications screen: which medicines are current, the history grouped per medicine, and the code chips.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { codeChips, decisionOf, decisionReasonOf, supplementState, historyGroups, isCurrent, pgxFor, replacedReason, supplementRows, type MedRecord } from '../src/lib/logic/meds.ts';

const rec = (id: string, detail: Record<string, unknown>, extra: Partial<MedRecord> = {}): MedRecord => ({
	id,
	title: String(detail.name ?? ''),
	detail,
	status: 'current',
	status_reason: null,
	supersedes: null,
	recorded_at: '2026-11-01T10:00:00Z',
	...extra
});

test('a medicine is current between its start and its stop, as the server counts it', () => {
	const today = '2026-11-10';
	assert.equal(isCurrent(rec('a', { name: 'A', med_status: 'active', started: '2026-03-10' }), today), true);
	assert.equal(isCurrent(rec('b', { name: 'B', med_status: 'stopped', started: '2022-06-20', stopped: '2025-12-01' }), today), false);
	assert.equal(isCurrent(rec('c', { name: 'C', med_status: 'active', started: '2026-12-01' }), today), false, 'starts later');
	assert.equal(isCurrent(rec('d', { name: 'D', med_status: 'on_hold', started: '2026-01-01' }), today), false);
	assert.equal(isCurrent(rec('e', { name: 'E', med_status: 'active' }), today), true, 'no dates');
	assert.equal(isCurrent(rec('f', { name: 'F', med_status: 'active' }, { status: 'superseded' }), today), false);
	assert.equal(isCurrent(rec('g', { name: 'G', med_status: 'stopped', started: '2026-01-01', stopped: '2026-12-01' }), today), true, 'stops later');
});

test('the history groups the pause and the restart of one medicine, newest first', () => {
	const groups = historyGroups(
		[
			rec('omep', { name: 'Omeprazole', ingredient: 'omeprazole', started: '2026-04-01', med_status: 'active' }),
			rec('old', { name: 'Atorvastatin', ingredient: 'atorvastatin', started: '2022-06-20', stopped: '2025-12-01', med_status: 'stopped', reason_text: 'paused' }),
			rec('new', { name: 'Atorvastatin', ingredient: 'Atorvastatin ', started: '2026-03-10', med_status: 'active' }),
			rec('v1', { name: 'Vitamin D3', kind: 'supplement', started: '2025-10-01', med_status: 'active' }, { status: 'superseded', status_reason: 'dose corrected' })
		],
		'2026-11-10'
	);
	assert.deepEqual(
		groups.map((g) => [g.title, g.entries.map((e) => e.id), g.current, g.state]),
		[
			['Omeprazole', ['omep'], true, null],
			['Atorvastatin', ['new', 'old'], true, null],
			['Vitamin D3', ['v1'], false, null]
		]
	);
});

test('a medicine not taken today is labelled by its newest version, never stopped by default', () => {
	const today = '2026-11-10';
	const state = (...rows: MedRecord[]) => historyGroups(rows, today)[0].state;
	assert.equal(state(rec('s', { name: 'S', med_status: 'stopped', started: '2022-01-01', stopped: '2025-12-01' })), 'stopped');
	assert.equal(state(rec('h', { name: 'H', med_status: 'on_hold', started: '2026-01-01' })), 'on_hold');
	assert.equal(state(rec('f', { name: 'F', med_status: 'active', started: '2026-12-01' })), 'later', 'starts later');
	assert.equal(state(rec('r', { name: 'R', med_status: 'active', started: '2026-01-01' }, { status: 'retracted' })), 'retracted');
	assert.equal(
		state(
			rec('old', { name: 'P', med_status: 'stopped', started: '2022-01-01', stopped: '2025-01-01' }),
			rec('new', { name: 'P', med_status: 'on_hold', started: '2025-06-01' })
		),
		'on_hold',
		'the newest course decides'
	);
});

test('PGx answers show only for the profile they were asked for, and never on a supplement', () => {
	const views = { slug: 'alex', views: { atorvastatin: { pgx: [{ gene: 'SLCO1B1' }] } } };
	assert.deepEqual(pgxFor(views, 'alex', 'atorvastatin', 'medication'), { pgx: [{ gene: 'SLCO1B1' }] });
	assert.equal(pgxFor(views, 'sam', 'atorvastatin', 'medication'), null, 'another profile');
	assert.equal(pgxFor(views, 'alex', 'atorvastatin', 'supplement'), null, 'a supplement');
	assert.equal(pgxFor(null, 'alex', 'atorvastatin', 'medication'), null);
});

test('code chips show the primary code first and every received code once', () => {
	const chips = codeChips({
		code_system: 'rxnorm',
		code: '860975',
		codes: [
			{ system: 'https://example.org/fhir/sid/national-drug-code', code: '000000-0', display: 'Metformina 850 mg' },
			{ system: 'http://www.nlm.nih.gov/research/umls/rxnorm', code: '860975' },
			{ system: 'http://www.whocc.no/atc', code: 'A10BA02' }
		]
	});
	assert.deepEqual(
		chips.map((c) => `${c.label} ${c.code}`),
		['RxNorm 860975', 'example.org 000000-0', 'ATC A10BA02']
	);
	assert.equal(chips[1].display, 'Metformina 850 mg');
	assert.deepEqual(codeChips({}), []);
	assert.deepEqual(codeChips({ codes: 'not a list' }), []);
});

test('an agent proposal reason splits into a translatable action and the reason text', () => {
	assert.deepEqual(replacedReason('agent proposal stop'), { action: 'stop', text: null });
	assert.deepEqual(replacedReason('agent proposal change: moved to the morning'), { action: 'change', text: 'moved to the morning' });
	assert.deepEqual(replacedReason('dosis mal copiada'), { action: null, text: 'dosis mal copiada' });
	assert.deepEqual(replacedReason(null), { action: null, text: null });
});

test('the supplements card lists every live supplement, taken ones first, and reads only known decisions', () => {
	const rows = supplementRows(
		[
			rec('iron', { name: 'Iron', kind: 'supplement', started: '2025-09-01', stopped: '2025-12-15', med_status: 'stopped', decision: 'do_not_repurchase' }),
			rec('d3', { name: 'Vitamin D3', kind: 'supplement', started: '2026-01-01', med_status: 'active' }),
			rec('d3old', { name: 'Vitamin D3', kind: 'supplement', started: '2025-01-01' }, { status: 'superseded' }),
			rec('mag', { name: 'Magnesium', kind: 'supplement', med_status: 'active' }, { status: 'retracted' }),
			rec('statin', { name: 'Atorvastatin', kind: 'medication', med_status: 'active' })
		],
		'2026-11-10'
	);
	assert.deepEqual(
		rows.map((r) => r.id),
		['d3', 'iron']
	);
	assert.equal(decisionOf(rows[1].detail), 'do_not_repurchase');
	assert.equal(decisionOf(rows[0].detail), null);
	assert.equal(decisionOf({ decision: 'buy_more' }), null, 'a value we do not know shows no decision');
	assert.equal(decisionReasonOf({ decision_reason: { trust: 'untrusted_text', origin: 'person_note', text: 'Upset stomach' } }), 'Upset stomach', 'a read marks the reason');
	assert.equal(decisionReasonOf({ decision_reason: 'Upset stomach' }), 'Upset stomach');
	assert.equal(decisionReasonOf({ decision_reason: null }), null);
	const sup = (detail: Record<string, unknown>) => ({ id: 'x', status: 'current', title: 'X', detail: { kind: 'supplement', ...detail } }) as MedRecord;
	assert.equal(supplementState(sup({ med_status: 'active', started: '2026-12-01' }), '2026-11-10'), 'later', 'a future start is not stopped');
	assert.equal(supplementState(sup({ med_status: 'active', started: '2026-01-01' }), '2026-11-10'), 'active');
	assert.equal(supplementState(sup({ med_status: 'on_hold' }), '2026-11-10'), 'on_hold');
	assert.equal(supplementState(sup({ med_status: 'active', started: '2025-11-01', stopped: '2026-03-31' }), '2026-11-10'), 'stopped');
	assert.equal(decisionReasonOf({ decision_reason: { trust: 'untrusted_text', origin: 'person_note', text: ' ' } }), null);
});
