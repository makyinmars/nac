import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

test('SSH shim does not forward local Claude credentials or config', () => {
  const dir = mkdtempSync(join(tmpdir(), 'nac-claude-shim-'));
  try {
    const ssh = join(dir, 'ssh');
    writeFileSync(ssh, '#!/bin/sh\n[ -z "${CLAUDE_CODE_OAUTH_TOKEN+x}" ] && [ -z "${ANTHROPIC_API_KEY+x}" ] && [ -z "${CLAUDE_CONFIG_DIR+x}" ] || exit 77\nprintf "%s\\n" "$*"\n');
    chmodSync(ssh, 0o700);
    const remote = {
      host: 'example.invalid', cwd: '/workspace', executable: '/usr/bin/claude',
      config_dir: '~/custom-claude', wrapper: 'echo supervised',
      pidfile: '~/.cache/nac/exec/test.pid', ssh_args: [],
    };
    const output = spawnSync('node', [fileURLToPath(new URL('./remote-cli.mjs', import.meta.url))], {
      encoding: 'utf8',
      env: {
        ...process.env, PATH: `${dir}:${process.env.PATH}`,
        NAC_CLAUDE_REMOTE: JSON.stringify(remote),
        ANTHROPIC_API_KEY: 'never-forward-api',
        CLAUDE_CODE_OAUTH_TOKEN: 'never-forward-oauth',
        CLAUDE_CONFIG_DIR: '/local-config',
      },
    });
    assert.equal(output.status, 0, output.stderr);
    assert.match(output.stdout, /CLAUDE_CONFIG_DIR/);
    assert.doesNotMatch(output.stdout, /never-forward|local-config/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
