// Offline unit tests for the qmd sidecar's pure helpers — the parts that don't
// need the native @tobilu/qmd stack. The qmd-driven subcommands (retrieve/embed/
// status) are exercised only by the live smoke. The module's qmd import is
// dynamic, so importing it here loads without qmd installed. Run with
// `node --test host-rag/test/qmd-sidecar.test.js`.

import test from 'node:test';
import assert from 'node:assert/strict';

import { clampTopN, mapHits, sanitizeVecQuery, lexTokens, buildLexQuery } from '../qmd-sidecar.mjs';

test('clampTopN bounds and defaults', () => {
  assert.equal(clampTopN(undefined), 8);
  assert.equal(clampTopN('not a number'), 8);
  assert.equal(clampTopN(0), 1);
  assert.equal(clampTopN(3), 3);
  assert.equal(clampTopN(999), 50);
  assert.equal(clampTopN(4.9), 4);
});

test('mapHits projects the wire shape and caps bestChunk at 2000 chars', () => {
  const long = 'x'.repeat(5000);
  const hits = mapHits([
    { displayPath: 'memory/mem_a.md', title: 'A', bestChunk: long, score: 0.9, docid: 1, context: 'ctx' },
  ]);
  assert.equal(hits.length, 1);
  assert.equal(hits[0].displayPath, 'memory/mem_a.md');
  assert.equal(hits[0].bestChunk.length, 2000);
  assert.equal(hits[0].score, 0.9);
});

test('mapHits tolerates a missing bestChunk', () => {
  const hits = mapHits([{ displayPath: 'm.md', title: 'T', score: 0.1, docid: 2 }]);
  assert.equal(hits[0].bestChunk, '');
});

test('sanitizeVecQuery collapses newlines to a single line', () => {
  assert.equal(sanitizeVecQuery('open the\npage\r\nand read'), 'open the page and read');
  assert.equal(sanitizeVecQuery('  many   spaces \t here  '), 'many spaces here');
});

test('sanitizeVecQuery neutralizes negation that qmd rejects (incl. hyphenated words)', () => {
  // qmd's validateSemanticQuery rejects /-\w/ and /-"/; these would otherwise throw.
  for (const q of ['report -crash', 'the sub-agent and fail-open path', 'use -"quoted"']) {
    const out = sanitizeVecQuery(q);
    assert.ok(!/-\w/.test(out), `no -word in: ${out}`);
    assert.ok(!/-"/.test(out), `no -quote in: ${out}`);
  }
  assert.equal(sanitizeVecQuery('sub-agent'), 'sub agent');
});

test('sanitizeVecQuery leaves a lone dash and empty input alone', () => {
  assert.equal(sanitizeVecQuery('a - b'), 'a - b');
  assert.equal(sanitizeVecQuery(''), '');
  assert.equal(sanitizeVecQuery(undefined), '');
});

test('lexTokens extracts, dedups (case-insensitive), and caps', () => {
  assert.deepEqual(lexTokens('Open the Example domain OPEN'), ['Open', 'the', 'Example', 'domain']);
  assert.deepEqual(lexTokens('one two three four', 2), ['one', 'two']);
  assert.deepEqual(lexTokens('-- !! ??'), []);
});

test('buildLexQuery double-quotes tokens and OR-joins them', () => {
  assert.equal(buildLexQuery('alpha beta'), '"alpha" OR "beta"');
  // Tokens are quoted so an FTS operator word is treated as a literal, not OR/NEAR.
  assert.equal(buildLexQuery('find OR near'), '"find" OR "OR" OR "near"');
  assert.equal(buildLexQuery('   '), '');
});
