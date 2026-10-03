import { createProject, createSession, expect, test } from "./harness";

test("creates a local Claude chat from the production UI", async ({ harness, page, request }) => {
  const projectId = await createProject(request, harness);
  await page.route("**/claude/status?*", (route) =>
    route.fulfill({
      json: { available: true, authenticated: true, version: "2.1.283", reason: null },
    }),
  );
  await page.goto(`${harness.baseUrl}/#/project/${projectId}`);
  const dialog = page.getByRole("dialog", { name: "New Chat" });
  await dialog.getByRole("radio", { name: /^Claude Agent / }).click();
  await expect(dialog.getByText("Claude Code 2.1.283 is ready on this machine.")).toBeVisible();
  await expect(dialog.getByRole("radio", { name: /^Direct coding agent / })).toHaveCount(0);
  await expect(dialog.getByText("Primary model", { exact: true })).toHaveCount(0);
  await expect(dialog.getByRole("button", { name: "Create chat" })).toBeDisabled();
  await dialog.getByRole("checkbox", { name: /I trust this workspace/ }).check();
  await dialog.getByRole("button", { name: "Create chat" }).click();
  await expect(page).toHaveURL(/\/session\/[^/]+\/sessions$/);
  await expect(page.getByText("Claude Agent", { exact: true }).first()).toBeVisible();
  const sessionId = page.url().match(/\/session\/([^/]+)\//)?.[1];
  expect(sessionId).toBeTruthy();
  const snapshot = await request.get(`${harness.baseUrl}/sessions/${sessionId}`);
  expect(snapshot.ok()).toBe(true);
  expect(await snapshot.json()).toMatchObject({
    metadata: { agent_runtime: "claude-agent", behavior: "direct" },
  });
  await page.getByRole("button", { name: "Session settings" }).click();
  await expect(page.getByRole("dialog", { name: "Session settings" })).toContainText(
    "Claude Agent (fixed for this chat)",
  );
});

test("shows the selected SSH host in Claude status without local fallback", async ({
  harness,
  page,
  request,
}) => {
  const projectId = await createProject(request, harness);
  let checkedHost = "";
  let launchRequest: Record<string, unknown> | null = null;
  await page.route("**/projects", async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    const response = await route.fetch();
    const body = (await response.json()) as {
      projects: Array<Record<string, unknown> & { project_id: string }>;
    };
    return route.fulfill({
      json: {
        ...body,
        projects: body.projects.map((project) =>
          project.project_id === projectId
            ? { ...project, ssh_host: "ssh.example.test", ssh_port: 2222 }
            : project,
        ),
      },
    });
  });
  await page.route("**/claude/status?*", (route) => {
    const url = new URL(route.request().url());
    checkedHost = url.searchParams.get("ssh_host") ?? "";
    return route.fulfill({
      json: { available: true, authenticated: true, version: "2.1.283", reason: null },
    });
  });
  await page.route("**/sessions", (route) => {
    if (route.request().method() !== "POST") return route.continue();
    launchRequest = route.request().postDataJSON() as Record<string, unknown>;
    return route.fulfill({ status: 503, json: { error: "SSH host unavailable" } });
  });
  await page.goto(`${harness.baseUrl}/#/project/${projectId}`);
  const dialog = page.getByRole("dialog", { name: "New Chat" });
  await dialog.getByRole("radio", { name: /^Claude Agent / }).click();
  await expect(dialog.getByText("Claude Code 2.1.283 is ready on ssh.example.test.")).toBeVisible();
  expect(checkedHost).toBe("ssh.example.test");
  await dialog.getByRole("checkbox", { name: /I trust this workspace/ }).check();
  await dialog.getByRole("button", { name: "Create chat" }).click();
  await expect
    .poll(() => launchRequest)
    .toMatchObject({
      project_id: projectId,
      agent_runtime: "claude-agent",
      behavior: "direct",
      claude_trusted_workspace: true,
    });
  await expect(dialog).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/project/${projectId}$`));
});

test("trusts a NAC workspace and answers a Claude worker approval", async ({
  harness,
  page,
  request,
}) => {
  const sessionId = await createSession(request, harness, "orchestrator");
  await page.goto(`${harness.baseUrl}/#/session/${sessionId}/sessions`);
  await page.getByRole("button", { name: "Session settings" }).click();
  const settings = page.getByRole("dialog", { name: "Session settings" });
  await expect(settings.getByText("Claude worker workspace")).toBeVisible();
  await settings.getByRole("button", { name: "Trust workspace for Claude workers" }).click();
  await expect(settings.getByText("Trusted for Claude worker dispatches")).toBeVisible();
  const sessions = await request.get(`${harness.baseUrl}/sessions`);
  const listed = (await sessions.json()) as Array<{
    summary: { session_id: string; claude_worker_trusted_workspace: boolean };
  }>;
  expect(listed.find((entry) => entry.summary.session_id === sessionId)?.summary).toMatchObject({
    claude_worker_trusted_workspace: true,
  });
  await settings.getByRole("button", { name: "Close" }).click();

  let reply: unknown = null;
  await page.route(`**/sessions/${sessionId}/claude-permissions`, (route) =>
    route.fulfill({
      json: [
        {
          id: "approval-e2e",
          claude_request_id: "native-approval-e2e",
          session_id: sessionId,
          run_id: "dispatch-e2e",
          generation: 3,
          tool_name: "Write",
          input_preview: '{"file_path":"notes.txt"}',
          created_at_epoch_ms: Date.now(),
        },
      ],
    }),
  );
  await page.route(
    `**/sessions/${sessionId}/claude-permissions/approval-e2e/reply`,
    async (route) => {
      reply = route.request().postDataJSON();
      return route.fulfill({ status: 200 });
    },
  );
  const approval = page.getByRole("dialog", { name: "Claude Agent permission required" });
  await expect(approval).toContainText("notes.txt");
  await approval.getByRole("button", { name: "Allow once" }).click();
  await expect
    .poll(() => reply)
    .toEqual({
      run_id: "dispatch-e2e",
      generation: 3,
      reply: "allow_once",
    });
});
