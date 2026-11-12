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

// The lane's axis ends today: a course that has not started yet is not drawn at today's edge.
import { laneBars } from '../src/lib/logic/lane.ts';

test('a course that starts after today has no bar, and each bar keeps its own segment', () => {
	const segs = [
		{ date: '2026-03-10', end: null, label: 'Atorvastatin 20 mg' },
		{ date: '2027-01-15', end: null, label: 'Atorvastatin 40 mg' }
	];
	const rows = laneBars(segs, x, '2026-11-10');
	assert.deepEqual(
		rows.map((r) => r.g.label),
		['Atorvastatin 20 mg']
	);
	assert.equal(rows[0].b.x2, x('2026-11-10'));
	assert.deepEqual(laneBars([segs[1]], x, '2026-11-10'), []);
});

// The lane's axis starts at the first course drawn: a profile whose courses all start after today has no lane.
import { buildLane } from '../src/lib/logic/lane.ts';

test('a lane with only courses after today is no lane, and a later course never moves the axis', () => {
	const future = { type: 'medication' as const, label: 'Ibuprofen', segments: [{ date: '2099-03-01', end: null, label: 'Ibuprofen 200 mg' }] };
	assert.equal(buildLane([future], '2026-11-10'), null);
	const now = { type: 'diagnosis' as const, label: 'Asthma', segments: [{ date: '2024-05-02', end: null, label: 'Asthma' }] };
	const lane = buildLane([future, now], '2026-11-10');
	assert.ok(lane);
	assert.deepEqual(
		lane.rows.map((r) => r.span.label),
		['Asthma']
	);
	assert.deepEqual(
		lane.ticks.map((k) => k.year),
		[2024, 2025, 2026]
	);
});
