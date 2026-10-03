import { expect, it } from "vitest";

import { PROVIDER_KINDS, providerLabel, providerUsesApiKey } from "@/app/lib/providers";

it("exposes OpenAI Chat Completions as a keyed first-class provider", () => {
  expect(PROVIDER_KINDS).toContain("openai-chat-completions");
  expect(providerLabel("openai-chat-completions")).toBe("OpenAI Chat Completions");
  expect(providerUsesApiKey("openai-chat-completions")).toBe(true);
});

it("labels Claude Agent sessions without offering the runtime as a NAC model provider", () => {
  expect(providerLabel("claude-agent")).toBe("Claude Agent");
  expect(PROVIDER_KINDS).not.toContain("claude-agent");
  expect(providerUsesApiKey("claude-agent")).toBe(false);
});
