// Unit tests for the orchestrator path's offline-testable, SDK-free helpers:
// `specialistsFromEnv` (parsing the host-supplied CLAW_SPECIALISTS menu) and
// `buildSystemPrompt` (the data-driven persona). The full turn (runClaudeTurn)
// drives the Agent SDK and is exercised only by the live smoke. Run with
// `node --test shim/test/claude.test.js`.

import test from 'node:test';
import assert from 'node:assert/strict';

import { specialistsFromEnv, buildSystemPrompt } from '../src/claude.js';

test('specialistsFromEnv returns an empty list when the env var is absent', () => {
  assert.deepEqual(specialistsFromEnv({}), []);
});

test('specialistsFromEnv parses a JSON array of name/description entries', () => {
  const out = specialistsFromEnv({
    CLAW_SPECIALISTS: JSON.stringify([
      { name: 'browser', description: 'browses the web and reads pages' },
    ]),
  });
  assert.deepEqual(out, [{ name: 'browser', description: 'browses the web and reads pages' }]);
});

test('specialistsFromEnv tolerates malformed JSON and non-arrays by returning empty', () => {
  assert.deepEqual(specialistsFromEnv({ CLAW_SPECIALISTS: 'not json' }), []);
  assert.deepEqual(specialistsFromEnv({ CLAW_SPECIALISTS: JSON.stringify({ a: 1 }) }), []);
});

test('specialistsFromEnv drops entries missing a string name or description', () => {
  const out = specialistsFromEnv({
    CLAW_SPECIALISTS: JSON.stringify([
      { name: 'browser', description: 'ok' },
      { name: 'noDesc' },
      { description: 'no name' },
      { name: 5, description: 'bad name type' },
    ]),
  });
  assert.deepEqual(out, [{ name: 'browser', description: 'ok' }]);
});

test('buildSystemPrompt omits delegation framing when no specialists are registered', () => {
  const prompt = buildSystemPrompt([]);
  assert.doesNotMatch(prompt, /- delegate:/);
  assert.doesNotMatch(prompt, /Specialists you can delegate to:/);
  // Preserves the "a specialist can be set up" framing, and explicitly instructs
  // the bot NOT to ask the user to paste the content in.
  assert.match(prompt, /has not been set up yet/);
  assert.match(prompt, /do not ask the user to paste in the content/);
});

test('buildSystemPrompt lists each registered specialist by name and description', () => {
  const prompt = buildSystemPrompt([
    { name: 'browser', description: 'browses the web and reads pages' },
    { name: 'sql', description: 'runs read-only database queries' },
  ]);
  assert.match(prompt, /- delegate:/);
  assert.match(prompt, /Specialists you can delegate to:/);
  assert.match(prompt, /- browser: browses the web and reads pages/);
  assert.match(prompt, /- sql: runs read-only database queries/);
  // Keeps the "present delegated work as your own" guidance.
  assert.match(prompt, /never say "delegate", "specialist", or "sub-agent"/);
});
