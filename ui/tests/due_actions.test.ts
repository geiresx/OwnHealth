// The Due screen's sections once a person acts: an item whose snooze ran out is listed under "Shown
// again", in one section only, and a new snooze starts no earlier than tomorrow.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { addDays, docLabel, dueGroups, evidenceDay, evidenceLimit, shownAgain, type DueItem } from '../src/lib/logic/due.ts';

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

test('an open item whose snooze ran out is shown again, once', () => {
	const again = item('a', 'due', { decision: { kind: 'snooze', until: '2026-12-01' } });
	const open = item('b', 'overdue');
	const done = item('c', 'up_to_date', { decision: { kind: 'done', evidence: { table: 'procedure', id: 'x' } } });
	const na = item('d', 'not_assessed', { answer: 'smoking_history' });
	const groups = dueGroups([open, again, done, na]);
	assert.deepEqual(
		groups.map((g) => [g.id, g.rows.map((r) => r.key)]),
		[
			['again', ['a']],
			['open', ['b']],
			['done', ['c']],
			['na', ['d']]
		]
	);
	assert.equal(shownAgain(again), true);
	// a snoozed item that is up to date now is just up to date
	assert.equal(shownAgain(item('e', 'up_to_date', { decision: { kind: 'snooze', until: '2026-12-01' } })), false);
	// every item lands in exactly one section
	const all = groups.flatMap((g) => g.rows);
	assert.equal(all.length, 4);
});

test('days add across a month and a year end', () => {
	assert.equal(addDays('2026-11-30', 1), '2026-12-01');
	assert.equal(addDays('2026-12-31', 1), '2027-01-01');
	assert.equal(addDays('2026-11-12', 90), '2027-02-10');
});

test('a document in the Done evidence list is labelled by its file name text, never an object', () => {
	const untrusted = { id: 'd1a2b3c4-0000', filename: { trust: 'untrusted_text', origin: 'document', text: 'LabNorte_2026-11-03.pdf' }, doc_type: 'lab_report', collected_at: '2026-11-03T08:00:00Z' };
	assert.equal(docLabel(untrusted), 'LabNorte_2026-11-03.pdf · 2026-11-03');
	assert.equal(docLabel({ id: 'd1a2b3c4-0000', filename: 'scan.pdf', collected_at: null }), 'scan.pdf');
	assert.equal(docLabel({ id: 'd1a2b3c4-0000', filename: null, doc_type: 'lab_report' }), 'lab_report');
	assert.equal(docLabel({ id: 'd1a2b3c4-0000', filename: { text: '' } }), 'd1a2b3c4');
	for (const d of [untrusted, { id: 'x', filename: { text: 'a.pdf' } }]) assert.ok(!docLabel(d).includes('[object'));
});

test('evidence counts only with a day on or before today, as the server checks it', () => {
	assert.equal(evidenceDay('2026-11-01', '2026-11-13'), '2026-11-01');
	assert.equal(evidenceDay('2026-11-13T08:00:00Z', '2026-11-13'), '2026-11-13');
	assert.equal(evidenceDay('2026-11-14', '2026-11-13'), null);
	assert.equal(evidenceDay(undefined, '2026-11-13'), null);
	assert.equal(evidenceDay('', '2026-11-13'), null);
});

test('the evidence limit is the UTC day the server checks, even where the local day is already the next one', () => {
	// 00:30 in Madrid on 13 November is still 12 November on the server
	const now = new Date('2026-11-12T23:30:00Z');
	assert.equal(evidenceLimit(now), '2026-11-12');
	assert.equal(evidenceDay('2026-11-13', evidenceLimit(now)), null);
	assert.equal(evidenceLimit(new Date('2026-11-13T00:30:00Z')), '2026-11-13');
});
