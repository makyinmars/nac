import assert from 'node:assert/strict';
import test from 'node:test';
import { createInitGate } from './init-gate.mjs';

test('ordinary output needs a native init and duplicate init is rejected', () => {
  const gate = createInitGate();
  assert.equal(gate({ type: 'system', subtype: 'hook_started' }), 'defer');
  assert.equal(gate({ type: 'system', subtype: 'hook_response' }), 'defer');
  assert.equal(gate({ type: 'rate_limit_event' }), 'defer');
  assert.throws(() => gate({ type: 'result', result: 'unsafe answer' }), /before native/);
  assert.equal(gate({ type: 'system', subtype: 'init', session_id: 'native-id' }), 'init');
  assert.equal(gate({ type: 'assistant', message: {} }), 'event');
  assert.throws(() => gate({ type: 'system', subtype: 'init', session_id: 'other' }), /duplicate/);
});
