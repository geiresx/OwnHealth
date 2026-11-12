// The doctor summary screen: options chosen for one person never carry over to another.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const page = readFileSync(new URL('../src/routes/summary/+page.svelte', import.meta.url), 'utf8');

test('summary: a change of profile clears the sensitive tick, so it is chosen again for each person', () => {
	const effects = [...page.matchAll(/\$effect\(\(\) => \{([\s\S]*?)\n\t\}\);/g)].map((m) => m[1]);
	const onProfile = effects.filter((body) => /void prof\?\.slug;/.test(body));
	assert.equal(onProfile.length, 1, 'one effect runs on a change of profile');
	assert.match(onProfile[0], /\bsensitive = false;/);
});
