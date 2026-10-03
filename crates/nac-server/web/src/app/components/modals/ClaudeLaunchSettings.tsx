import { Input, InputSize } from "@/app/atoms";
import { useClaudeStatus } from "@/app/services/queries/claude";
import type { SshTarget } from "@/app/types/api";

export interface ClaudeLaunchSelection {
  executable: string;
  model: string;
  configDir: string;
  trustedWorkspace: boolean;
}

export function ClaudeLaunchSettings({
  value,
  onChange,
  host,
  enabled = true,
}: {
  value: ClaudeLaunchSelection;
  onChange: (value: ClaudeLaunchSelection) => void;
  host: SshTarget | null;
  enabled?: boolean;
}) {
  const status = useClaudeStatus(host, value, enabled);
  return (
    <div className="flex flex-col gap-4">
      <div role="status" className="text-micro text-basic-secondary">
        {status.isPending
          ? `Checking Claude Code on ${host ? host.ssh_host : "this machine"}…`
          : status.isError
            ? `Claude Code status unavailable: ${status.error.message}`
            : status.data?.available && status.data.authenticated
              ? `Claude Code ${status.data.version ?? ""} is ready on ${host ? host.ssh_host : "this machine"}.`
              : (status.data?.reason ??
                "Claude Code needs installation or subscription login on this host.")}
      </div>
      <label className="flex flex-col gap-1 text-micro text-basic-primary">
        Claude executable on this host
        <Input
          inputSize={InputSize.Medium}
          value={value.executable}
          placeholder="claude"
          onChange={(event) => onChange({ ...value, executable: event.target.value })}
        />
      </label>
      <label className="flex flex-col gap-1 text-micro text-basic-primary">
        Claude model (optional)
        <Input
          inputSize={InputSize.Medium}
          value={value.model}
          placeholder="Host default"
          onChange={(event) => onChange({ ...value, model: event.target.value })}
        />
      </label>
      <label className="flex flex-col gap-1 text-micro text-basic-primary">
        CLAUDE_CONFIG_DIR on this host (optional)
        <Input
          inputSize={InputSize.Medium}
          value={value.configDir}
          placeholder="Host default"
          onChange={(event) => onChange({ ...value, configDir: event.target.value })}
        />
      </label>
      <label className="flex items-start gap-2 text-micro text-basic-secondary">
        <input
          type="checkbox"
          checked={value.trustedWorkspace}
          onChange={(event) => onChange({ ...value, trustedWorkspace: event.target.checked })}
        />
        I trust this workspace for Claude Code file operations.
      </label>
      <p className="text-micro text-basic-muted">
        Claude user, project, and local settings are disabled here. Project instructions, hooks, and
        MCP servers from those settings do not load.
      </p>
      <p className="text-micro text-basic-muted">
        Claude keeps native transcripts on the execution host. Removing the NAC chat does not remove
        those files.
      </p>
    </div>
  );
}
