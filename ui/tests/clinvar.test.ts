// Unit tests for the ClinVar helpers: the Settings card (logic/inbox.ts), what the switch shows before the
// household has its source and what a save sends, and the Genome subtitle's release (logic/genome.ts).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { clinvarConfig, clinvarOn } from '../src/lib/logic/inbox.ts';
import { releaseParts } from '../src/lib/logic/genome.ts';

test('before the first genome the switch shows the default: on, except in the demo', () => {
	assert.equal(clinvarOn(null, false), true);
	assert.equal(clinvarOn(null, true), false);
	assert.equal(clinvarOn({ enabled: false }, false), false);
	assert.equal(clinvarOn({ enabled: true }, true), true);
});

test('a save keeps a mirror set through the API unless a path is typed', () => {
	const mirror = { url: 'http://mirror.lan/clinvar.vcf.gz', allow_private: true };
	assert.deepEqual(clinvarConfig('', mirror), mirror);
	assert.deepEqual(clinvarConfig('  ', { url: 'https://mirror.example/c.vcf.gz' }), { url: 'https://mirror.example/c.vcf.gz' });
	assert.deepEqual(clinvarConfig(' /srv/c.vcf.gz ', mirror), { path: '/srv/c.vcf.gz' });
	assert.deepEqual(clinvarConfig('', { path: '/srv/old.vcf' }), {});
	assert.deepEqual(clinvarConfig('', undefined), {});
});

test('the Genome subtitle names the ClinVar release in use, also when the genome file names none', () => {
	assert.deepEqual(releaseParts({ clinvar: '2026-09-28', cpic: '2026-09' }, null), ['ClinVar 2026-09-28', 'CPIC 2026-09']);
	assert.deepEqual(releaseParts({ clinvar: '2026-09-28', cpic: '2026-09' }, '2026-10-05'), ['ClinVar 2026-10-05', 'CPIC 2026-09']);
	assert.deepEqual(releaseParts({ cpic: '2026-09', clinvar: '2026-09-28' }, '2026-10-05'), ['CPIC 2026-09', 'ClinVar 2026-10-05']);
	assert.deepEqual(releaseParts({ cpic: '2026-09' }, '2026-10-05'), ['CPIC 2026-09', 'ClinVar 2026-10-05']);
	assert.deepEqual(releaseParts({ cpic: '2026-09', pharmcat: '3.4.0' }, null), ['CPIC 2026-09']);
	assert.deepEqual(releaseParts({}, null), []);
});
