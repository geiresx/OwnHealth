// The Due card and the Due screen: one list from the prevention document and the older lab-gap rows,
// with nothing shown twice.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dueCard, dueCardView, dueColor, dueScreenOpen, isOpen, lockedScopes, partialScopes, type DueItem, type GlanceDue } from '../src/lib/logic/due.ts';

const item = (key: string, status: DueItem['status'], extra: Partial<DueItem> = {}): DueItem => ({
	key,
	rule: key,
	area: 'screening',
	kind: 'every_n_months',
	status,
	src: [],
	title: key,
	why: 'why',
	...extra
});
const hcvRow: GlanceDue = { title: 'Hepatitis C antibody', why: 'never', status: 'watch', kind: 'lab', phase: null, rule: 'infect.hcv.once_adult' };
const thyroidRow: GlanceDue = { title: 'Retest TSH + FT4', why: 'stale', status: 'stale', kind: 'lab', phase: null, rule: 'recency.thyroid' };

test('a lab gap the prevention list holds shows once, from the prevention list', () => {
	const prevention = [
		item('es.cancer.breast', 'due'),
		item('infect.hcv.once_adult', 'due', { area: 'lab', kind: 'never_measured', title: 'Hepatitis C antibody' }),
		item('stale:recency.thyroid:tsh', 'overdue', { rule: 'recency.thyroid', area: 'lab', kind: 'recency' })
	];
	const rows = dueCard([hcvRow, thyroidRow], prevention);
	assert.deepEqual(
		rows.map((r) => r.key),
		['es.cancer.breast', 'infect.hcv.once_adult', 'stale:recency.thyroid:tsh']
	);
	assert.equal(rows.filter((r) => r.title === 'Hepatitis C antibody').length, 1);
});

test('without prevention items (no read:records) the older lab rows stay', () => {
	assert.deepEqual(
		dueCard([hcvRow, thyroidRow], []).map((r) => [r.title, r.color, r.word]),
		[
			['Hepatitis C antibody', 'watch', 'watch'],
			['Retest TSH + FT4', 'stale', 'stale']
		]
	);
	assert.equal(dueCard([hcvRow], undefined).length, 1, 'an older server without the key');
});

test('the card lists open items only, each with its colour and word', () => {
	const rows = dueCard([], [item('a', 'overdue'), item('b', 'up_to_date'), item('c', 'upcoming'), item('d', 'not_assessed')]);
	assert.deepEqual(
		rows.map((r) => [r.key, r.color, r.word]),
		[
			['a', 'out', 'due_st_overdue'],
			['c', 'stale', 'due_st_upcoming']
		]
	);
	assert.equal(isOpen({ status: 'due' }), true);
	assert.equal(isOpen({ status: 'up_to_date' }), false);
	assert.equal(dueColor('up_to_date'), 'ok');
	assert.equal(dueColor('not_assessed'), 'nodata');
});

test('an empty Due card that withholds items names the scopes, and a card with rows does not', () => {
	assert.equal(lockedScopes([], ['read:records']), 'read:records');
	assert.equal(lockedScopes([], ['read:labs', 'read:records']), 'read:labs, read:records');
	assert.equal(lockedScopes([], []), null);
	assert.equal(lockedScopes([], undefined), null);
	assert.equal(lockedScopes(dueCard([hcvRow], []), ['read:records']), null);
});

test('a Due card with rows still names the scopes of the items it leaves out', () => {
	assert.equal(partialScopes(dueCard([hcvRow], []), ['read:labs']), 'read:labs');
	assert.equal(partialScopes(dueCard([hcvRow], []), []), null);
	assert.equal(partialScopes(dueCard([hcvRow], []), undefined), null);
	assert.equal(partialScopes([], ['read:labs']), null);
});

test('the Due card links to the Due screen only when the caller can read what it lists', () => {
	assert.equal(dueScreenOpen(undefined), true);
	assert.equal(dueScreenOpen([]), true);
	assert.equal(dueScreenOpen(['read:labs']), true);
	assert.equal(dueScreenOpen(['read:records']), false);
	assert.equal(dueScreenOpen(['read:labs', 'read:records']), false);
});

// The dashboard's Due card lists six rows. With the Due screen open to the caller the rest are a
// link away; without read:records there is no screen, so the card itself reveals them.
test('the Due card reveals its hidden rows when the Due screen is closed to the caller', () => {
	const rows = Array.from({ length: 8 }, (_, n) => ({ key: `k${n}`, title: `t${n}`, why: '', color: 'watch', word: 'due', area: 'lab', phase: null }));
	assert.deepEqual(
		[dueCardView(rows, 6, true, false)].map((v) => [v.rows.length, v.more, v.action]),
		[[6, 2, 'link']]
	);
	const shut = dueCardView(rows, 6, false, false);
	assert.deepEqual([shut.rows.length, shut.more, shut.action], [6, 2, 'expand']);
	const all = dueCardView(rows, 6, false, true);
	assert.deepEqual([all.rows.length, all.more, all.action], [8, 0, 'collapse']);
	const few = dueCardView(rows.slice(0, 4), 6, false, false);
	assert.deepEqual([few.rows.length, few.more, few.action], [4, 0, null]);
});
