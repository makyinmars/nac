import assert from 'node:assert/strict';
import test from 'node:test';
import { createApprovalGate } from './approval-gate.mjs';

test('one NAC approval binds the exact tool call across hook and permission callback', async () => {
  const seen = [];
  const gate = createApprovalGate(async (name, input) => {
    seen.push([name, input]);
    return { allow: true };
  });
  const hook = gate.hooks.PreToolUse[0].hooks[0];
  const input = { file_path: '/workspace/code.rs' };
  const decision = await hook({ tool_name: 'Edit', tool_input: input, tool_use_id: 'tool-1' }, 'tool-1', {});
  assert.equal(decision.hookSpecificOutput.permissionDecision, 'allow');
  assert.deepEqual(seen, [['Edit', input]]);
  assert.equal((await gate.canUseTool('Edit', input, { toolUseID: 'tool-1' })).behavior, 'allow');
  assert.equal((await gate.canUseTool('Edit', input, { toolUseID: 'tool-1' })).behavior, 'deny');
});

test('changed input and lost approval channel fail closed', async () => {
  const gate = createApprovalGate(async () => ({ allow: true }));
  const hook = gate.hooks.PreToolUse[0].hooks[0];
  await hook({ tool_name: 'Write', tool_input: { file_path: 'a' }, tool_use_id: 'tool-2' }, 'tool-2', {});
  assert.equal((await gate.canUseTool('Write', { file_path: 'b' }, { toolUseID: 'tool-2' })).behavior, 'deny');
  const lost = createApprovalGate(async () => { throw Error('disconnected'); });
  const denied = await lost.hooks.PreToolUse[0].hooks[0]({ tool_name: 'Read', tool_input: {}, tool_use_id: 'tool-3' }, 'tool-3', {});
  assert.equal(denied.hookSpecificOutput.permissionDecision, 'deny');
  assert.equal((await lost.canUseTool('Read', {}, { toolUseID: 'tool-3' })).behavior, 'deny');
});

test('auto-allowed tool completion releases its one-use approval', async () => {
  const gate = createApprovalGate(async () => ({ allow: true }));
  const input = { file_path: '/workspace/proof.txt' };
  const approved = await gate.hooks.PreToolUse[0].hooks[0](
    { tool_name: 'Read', tool_input: input, tool_use_id: 'tool-auto' }, 'tool-auto', {},
  );
  assert.equal(approved.hookSpecificOutput.permissionDecision, 'allow');
  await gate.hooks.PostToolUse[0].hooks[0]({ tool_use_id: 'tool-auto' }, 'tool-auto');
  assert.equal((await gate.canUseTool('Read', input, { toolUseID: 'tool-auto' })).behavior, 'deny');
});
