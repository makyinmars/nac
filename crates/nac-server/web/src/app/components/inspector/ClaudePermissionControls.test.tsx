/** @vitest-environment jsdom */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import { ClaudePermissionControls } from "@/app/components/inspector/ClaudePermissionControls";
import { ToastProvider } from "@/app/providers/ToastProvider";
import { api } from "@/app/services/api";

beforeEach(() => {
  vi.stubGlobal("matchMedia", () => ({
    matches: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  }));
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("shows a Claude tool request and replies for its exact run generation", async () => {
  vi.spyOn(api, "getClaudePermissions").mockResolvedValue([
    {
      id: "ask-1",
      claude_request_id: "native-ask-1",
      session_id: "session",
      run_id: "dispatch-7",
      generation: 4,
      tool_name: "Bash",
      input_preview: "git status",
      created_at_epoch_ms: 1,
    },
  ]);
  const reply = vi.spyOn(api, "replyClaudePermission").mockResolvedValue(undefined);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const view = render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <MemoryRouter>
          <ClaudePermissionControls sessionId="session" />
        </MemoryRouter>
      </ToastProvider>
    </QueryClientProvider>,
  );
  try {
    expect(await screen.findByText("git status")).toBeTruthy();
    expect(screen.getByText("Claude Agent permission required")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Allow once" }));
    await waitFor(() =>
      expect(reply).toHaveBeenCalledWith("session", "ask-1", "dispatch-7", 4, "allow_once"),
    );
  } finally {
    view.unmount();
    client.clear();
  }
});
