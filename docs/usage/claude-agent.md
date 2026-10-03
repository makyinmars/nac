# Claude Agent on a personal NAC installation

Claude Agent runs Claude Code with the subscription login on the execution
host. NAC does not ask for an Anthropic API key or copy Claude login files.
Each local or SSH host needs its own Claude Code installation and login.
The [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)
documents the login commands below.

## Prepare each host

1. Install Claude Code version 2.1.280 or newer on the host that will run it.
2. Run `claude --version` with the executable that NAC will use on that host.
3. Run `claude auth login` with that executable and a Claude subscription account.
4. Run `claude auth status` with that executable. It must report `loggedIn: true`,
   `authMethod: "claude.ai"`, and `apiProvider: "firstParty"`.

Run `make setup` in the NAC source checkout. This installs the locked Agent SDK
bridge on the NAC host. The current build reads that bridge from the source
checkout, so keep the checkout and its installed bridge files in place while
you use Claude Agent.

On this personal host, `claude` on the default `PATH` still points to the old
Homebrew 2.0.31 installation. The updated subscription CLI at
`/Users/franklin/.local/bin/claude` is version 2.1.283. Enter that path in the
Claude executable field, or put that directory before Homebrew on the NAC
process `PATH`. Top-level chats use the executable field. Claude workers run
`claude` from the execution host `PATH`, so start NAC with
`/Users/franklin/.local/bin` before Homebrew on `PATH` to use workers locally.
On each SSH host, make sure `claude` resolves to version 2.1.280 or newer in
the remote command environment. Choose the top-level executable separately
for each SSH host.

For SSH, make sure that NAC can connect to the selected host without an
interactive password prompt. The remote host needs `bash`, the Claude Code
executable, and its own subscription login. NAC uses the selected SSH identity
and workspace for launch, resume, and process cleanup. SSH port forwarding is
not required for approvals.

Remove `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`,
`CLAUDE_CODE_OAUTH_TOKEN`, and Claude Code cloud-provider overrides from the
execution host. For a local launch, NAC checks its own process environment;
for SSH, it checks the remote host and strips local credential variables from
the SSH subprocess. A subscription-backed Claude run refuses overrides on its
execution host instead of switching to API-key billing.

## Start a Claude chat

In a new project or **New Chat**, choose **Claude Agent** under **Agent
runtime**. NAC selects **direct** behavior. Choose the local or SSH workspace,
then select the Claude executable and optional model or `CLAUDE_CONFIG_DIR`.
If the host uses a nondefault Claude config directory, select that directory
explicitly; NAC does not inherit an ambient `CLAUDE_CONFIG_DIR` for a chat.
The status line reports Claude Code and login on that host. Make sure that you
trust the workspace before creating the chat. For subscription safety, NAC
disables Claude user, project, and local settings sources. Project `CLAUDE.md`,
hooks, and MCP servers from those sources do not load in this runtime. A
project settings file can otherwise inject an API key after NAC has removed
credential variables from the process environment.

The runtime, host, workspace, executable, and configuration directory are
pinned to the chat. Start another chat to change them. A missing Claude
installation or login produces an error for that host. NAC does not run the
chat locally when an SSH launch fails. Podman and managed hosts do not support
Claude Agent.

## Use Claude workers

In a NAC orchestrator chat, open **Session settings** and select **Trust
workspace for Claude workers**. The setting shows the host and folder before
you confirm. A NAC orchestrator can then dispatch a named thread with
`agent: "claude"`. Threads use NAC by default, and a named thread keeps its
chosen agent for later dispatches. Claude workers use the same host and
workspace as their parent orchestrator.
The worker card receives completed assistant activity and the final retained
handoff. Top-level chats show each assistant message after it is complete.

## Approvals and stored context

NAC exposes the Claude file tools `Read`, `Glob`, `Grep`, `Edit`, and `Write`.
NAC asks for **Allow once** or **Deny** before each exposed file tool
invocation. The approval applies to that invocation only. NAC native remembered
grants and automatic approval do not apply. NAC does not expose Bash or its
native tools through this bridge. If no one can answer, NAC denies the tool
request. NAC disables user, project, and local Claude settings sources so
those sources cannot inject credentials or start hooks and MCP processes.

Tool activity and usage updates stream while Claude works. NAC shows assistant
text after each complete message so it can redact credentials that span two
text chunks. If you cancel a run, NAC redacts any retained partial text before
adding it to the transcript.

For each file request, NAC checks the path on the selected local or SSH host
before showing the approval and checks it again after **Allow once**. It denies
paths outside the workspace, symbolic links below it, and direct edits to
Git metadata or the local NAC session store. Claude Code then performs `Edit`
and `Write` itself. Those writes do not use NAC's revision checks, prepared
no-follow file handles, or atomic mutation. Another process that changes a
path after NAC's final check could redirect a Claude write, including outside
the workspace. Use Claude Agent only in a workspace where you trust the other
processes and users that can change its files. The same timing gap applies to
Claude file reads.

NAC keeps its own transcript and run status. Claude Code also keeps native
transcripts on the execution host so it can resume context. Deleting a NAC
chat or thread does not delete those Claude files. NAC refuses resume if the
native transcript or pinned host and workspace are unavailable.
