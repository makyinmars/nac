#!/usr/bin/env node
// Agent SDK executable shim. All Claude protocol bytes stay on authenticated
// SSH stdio; the remote host supplies its own Claude binary and login.
import { spawn } from 'node:child_process';
import { cleanEnvironment } from './credential-env.mjs';

const remote = JSON.parse(process.env.NAC_CLAUDE_REMOTE || '{}');
const quote = (text) => `'${String(text).replaceAll("'", "'\\''")}'`;
const quotePath = (text) => text === '~' ? '~' : String(text).startsWith('~/')
  ? `~/${quote(String(text).slice(2))}` : quote(text);
if (!remote.host || !remote.cwd || !remote.executable || !remote.wrapper || !remote.pidfile) {
  process.stderr.write('Incomplete NAC SSH Claude target\n');
  process.exit(125);
}
const claude = [remote.executable, ...process.argv.slice(2)].map(quote).join(' ');
const config = remote.config_dir ? `config_dir=${quotePath(remote.config_dir)}; ` : '';
const configArgument = remote.config_dir ? 'CLAUDE_CONFIG_DIR="$config_dir" ' : '';
const requested = `${config}for name in $(env | cut -d= -f1); do case "$name" in CLAUDE_CODE_*_TOKEN|ANTHROPIC_*) unset "$name";; esac; done; env -u CLAUDE_CODE_HOST_SESSION_ID -u CLAUDE_CODE_USE_BEDROCK -u CLAUDE_CODE_USE_VERTEX -u CLAUDE_CODE_USE_FOUNDRY -u CLAUDE_CONFIG_DIR ${configArgument}${claude}`;
const pidfile = remote.pidfile.startsWith('~/')
  ? `"$HOME"/${quote(remote.pidfile.slice(2))}`
  : quote(remote.pidfile);
const command = `cd ${quotePath(remote.cwd)} && bash -lc ${quote(remote.wrapper)} nac-claude ${quote(requested)} ${pidfile}`;
const ssh = spawn('ssh', [...remote.ssh_args, '-T', '--', remote.host, command], {
  stdio: 'inherit',
  env: cleanEnvironment(process.env, true),
});
ssh.on('error', (error) => {
  process.stderr.write(`NAC SSH Claude launch failed: ${error.message}\n`);
  process.exitCode = 125;
});
ssh.on('exit', (code, signal) => {
  process.exitCode = signal ? 128 : (code ?? 125);
});
