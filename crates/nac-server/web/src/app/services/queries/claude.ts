import { useQuery } from "@tanstack/react-query";

import { api } from "@/app/services/api";
import type { SshTarget } from "@/app/types/api";

export function useClaudeStatus(
  host: SshTarget | null,
  selection: { executable: string; configDir: string },
  enabled: boolean,
) {
  return useQuery({
    queryKey: [
      "claude",
      "status",
      host?.ssh_host ?? "local",
      host?.ssh_port,
      host?.ssh_identity_file,
      selection.executable,
      selection.configDir,
    ],
    queryFn: ({ signal }) =>
      api.getClaudeStatus(host, selection.executable, selection.configDir, signal),
    enabled,
    staleTime: 30_000,
    retry: false,
  });
}
