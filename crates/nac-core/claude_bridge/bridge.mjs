#!/usr/bin/env node
// The only channel shared with NAC is bounded newline-delimited JSON. Claude's
// own wire protocol stays inside the Agent SDK, where canUseTool is supported.
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { query } from '@anthropic-ai/claude-agent-sdk';
import { createApprovalGate } from './approval-gate.mjs';
import { assertSubscriptionInit, cleanEnvironment } from './credential-env.mjs';
import { createInitGate } from './init-gate.mjs';

const pending = new Map();
let nextId = 0;
let inputClosed = false;
let initAck;
let start;
const ready = new Promise((resolve) => { start = resolve; });
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

lines.on('line', (line) => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.type === 'start') {
    start(message);
  } else if (message.type === 'init_ack') {
    initAck?.(message.ok === true);
    initAck = undefined;
  } else if (message.type === 'approval') {
    const settle = pending.get(message.id);
    if (settle) {
      pending.delete(message.id);
      settle(message);
    }
  }
});
lines.on('close', () => {
  inputClosed = true;
  for (const settle of pending.values()) settle({ allow: false, reason: 'NAC approval channel closed' });
  pending.clear();
  initAck?.(false);
  initAck = undefined;
});

const request = await ready;
if (inputClosed) process.exit(1);
const env = cleanEnvironment(process.env, Boolean(request.remote));
if (!request.config_dir) delete env.CLAUDE_CONFIG_DIR;
if (request.config_dir && !request.remote) env.CLAUDE_CONFIG_DIR = request.config_dir;
if (request.remote) env.NAC_CLAUDE_REMOTE = JSON.stringify(request.remote);

try {
  async function askNac(toolName, input, toolUseId, signal) {
    if (inputClosed) return { allow: false, reason: 'NAC approval channel closed' };
    const id = String(++nextId);
    const response = new Promise((resolve) => pending.set(id, resolve));
    send({ type: 'approval_request', id, tool_use_id: toolUseId, tool_name: toolName, input });
    const answer = await Promise.race([
      response,
      new Promise((resolve) => signal?.addEventListener('abort', () => resolve({ allow: false, reason: 'Run cancelled' }), { once: true })),
    ]);
    pending.delete(id);
    return answer;
  }
  const options = {
    cwd: request.cwd,
    pathToClaudeCodeExecutable: request.remote ? fileURLToPath(new URL('./remote-cli.mjs', import.meta.url)) : request.executable,
    model: request.model || undefined,
    resume: request.resume_id || undefined,
    includePartialMessages: true,
    permissionMode: 'default',
    // Only tools whose resource shape NAC can bind are exposed. This is a
    // model-facing capability list, not an authorization decision.
    tools: ['Read', 'Glob', 'Grep', 'Edit', 'Write'],
    // User/project/local settings can inject ANTHROPIC_API_KEY after SDK env
    // scrubbing, silently switching this subscription run to API billing.
    // The installed host login remains available with these sources disabled.
    settingSources: [],
    env,
    ...createApprovalGate(askNac),
  };
  const isNativeInit = createInitGate();
  for await (const message of query({ prompt: request.prompt, options })) {
    const order = isNativeInit(message);
    if (order === 'defer') continue;
    if (order === 'init') {
      assertSubscriptionInit(message);
      const acknowledged = new Promise((resolve) => { initAck = resolve; });
      send({ type: 'init', session_id: message.session_id, version: message.claude_code_version });
      if (!await acknowledged) throw Error('NAC did not persist Claude session identity');
    }
    send({ type: 'event', event: message });
    if (message.type === 'result') {
      const text = message.result || '';
      // Older Claude Code releases can label a model/API failure as a
      // successful SDK result. The visible error still must fail the run.
      const isError = message.subtype !== 'success' || message.is_error === true ||
        /^(?:API Error:|Spending cap reached\b)/.test(text);
      send({ type: 'result', session_id: message.session_id, subtype: message.subtype, result: text, is_error: isError });
      if (isError) process.exitCode = 1;
    }
  }
} catch (error) {
  send({ type: 'error', message: String(error?.message || error) });
  process.exitCode = 1;
} finally {
  lines.close();
  process.stdin.pause();
}
