// Unit tests for the generic specialist harness's one offline-testable helper:
// `specialistOptionsFromEnv`, which projects the host-supplied CLAW_SPECIALIST_*
// env into the turn's options. The turn itself (runSpecialistTurn) drives the
// Agent SDK and is exercised only by the live smoke, never the offline gate. Run
// with `node --test shim/test/specialist.test.js`.

import test from 'node:test';
import assert from 'node:assert/strict';

import { specialistOptionsFromEnv } from '../src/specialist.js';

test('reads system prompt, tools, allowed tools, and max turns from env', () => {
  const opts = specialistOptionsFromEnv({
    CLAW_SPECIALIST_SYSTEM_PROMPT: 'You are a web browsing specialist.',
    CLAW_SPECIALIST_TOOLS: JSON.stringify(['Bash']),
    CLAW_SPECIALIST_ALLOWED_TOOLS: JSON.stringify(['Bash(agent-browser:*)']),
    CLAW_SPECIALIST_MAX_TURNS: '25',
  });
  assert.equal(opts.systemPrompt, 'You are a web browsing specialist.');
  assert.deepEqual(opts.tools, ['Bash']);
  assert.deepEqual(opts.allowedTools, ['Bash(agent-browser:*)']);
  assert.equal(opts.maxTurns, 25);
});

test('defaults a missing system prompt to empty and missing tool lists to empty', () => {
  const opts = specialistOptionsFromEnv({});
  assert.equal(opts.systemPrompt, '');
  assert.deepEqual(opts.tools, []);
  assert.deepEqual(opts.allowedTools, []);
});

test('defaults max turns to 40 when absent or not a positive integer', () => {
  assert.equal(specialistOptionsFromEnv({}).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ CLAW_SPECIALIST_MAX_TURNS: 'nope' }).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ CLAW_SPECIALIST_MAX_TURNS: '0' }).maxTurns, 40);
  assert.equal(specialistOptionsFromEnv({ CLAW_SPECIALIST_MAX_TURNS: '-3' }).maxTurns, 40);
});

test('falls back to empty lists for malformed or non-array tool JSON', () => {
  assert.deepEqual(specialistOptionsFromEnv({ CLAW_SPECIALIST_TOOLS: 'not json' }).tools, []);
  assert.deepEqual(
    specialistOptionsFromEnv({ CLAW_SPECIALIST_TOOLS: JSON.stringify({ a: 1 }) }).tools,
    [],
  );
  assert.deepEqual(
    specialistOptionsFromEnv({ CLAW_SPECIALIST_ALLOWED_TOOLS: JSON.stringify([1, 2]) }).allowedTools,
    [],
  );
});
