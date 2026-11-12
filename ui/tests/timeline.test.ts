// The timeline's bars lane (slice 6): each row of `spans` goes through buildBars, the biomarker chart's helper.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildBars } from '../src/lib/logic/axis.ts';

const x = (d: string) => (Date.parse(d) - Date.parse('2021-01-01')) / 864e5 / 10; // one unit per 10 days

test('a condition with no abatement is a bar from its onset to today', () => {
	const [b] = buildBars([{ date: '2021-11-09', end: null, label: 'Hypercholesterolaemia' }], x, '2026-11-10');
	assert.deepEqual([b.span, b.ongoing, b.row], [true, true, 0]);
	assert.equal(b.x, x('2021-11-09'));
	assert.equal(b.x2, x('2026-11-10'));
});

test('a resolved condition with no abatement date is a one-day dot, never an open bar', () => {
	const [b] = buildBars([{ date: '2024-03-01', end: '2024-03-01', label: 'Bronchitis' }], x, '2026-11-10');
	assert.deepEqual([b.span, b.ongoing], [false, false]);
});

test('the courses of one medicine share its row: the pause is a gap, the current course is open', () => {
	const bars = buildBars(
		[
			{ date: '2022-06-20', end: '2025-12-01', label: 'Atorvastatin 20 mg' },
			{ date: '2026-03-10', end: null, label: 'Atorvastatin 20 mg' }
		],
		x,
		'2026-11-10'
	);
	assert.deepEqual(
		bars.map((b) => [b.row, b.ongoing]),
		[
			[0, false],
			[0, true]
		]
	);
	assert.ok(bars[0].x2 < bars[1].x, 'the pause shows as a gap');
});

// A visit's correction (ManualEntry): the form has only a day, the stored visit may carry times.
import { visitPeriod } from '../src/lib/logic/visit.ts';

test('correcting a visit without moving its day keeps the stored start, so an imported end stays valid', () => {
	const stored = { period_start: '2026-05-12T06:30:00Z', period_end: '2026-05-12T07:00:00Z' };
	assert.deepEqual(visitPeriod('2026-05-12', stored), { period_start: '2026-05-12T06:30:00Z' });
});

test('a new visit, or a moved day, is placed at noon UTC; an end before the new start is cleared', () => {
	assert.deepEqual(visitPeriod('2025-11-10'), { period_start: '2025-11-10T12:00:00Z' });
	const stored = { period_start: '2026-05-12T06:30:00Z', period_end: '2026-05-12T07:00:00Z' };
	assert.deepEqual(visitPeriod('2026-05-13', stored), { period_start: '2026-05-13T12:00:00Z', period_end: null });
	assert.deepEqual(visitPeriod('2026-05-11', stored), { period_start: '2026-05-11T12:00:00Z' });
	assert.deepEqual(visitPeriod(undefined, stored), {});
});
