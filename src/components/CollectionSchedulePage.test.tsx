import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { CollectionSchedulePage } from "./CollectionSchedulePage";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("renders server and remote collectors in one list and edits local plans", async () => {
  const requests: Array<{ method: string; url: string; body?: Record<string, unknown> }> = [];
  let targets = {
    targets: [
      { targetId: "server", kind: "server", displayName: "测试服务端", status: "online", includeMedia: true, dataRedaction: true, plans: [{ id: "plan-1", name: "早班采集", includeMedia: true, schedule: { mode: "interval", intervalMinutes: 30, dailyTime: "02:00" }, createdAt: "2026-09-21T01:00:00Z", updatedAt: "2026-09-21T01:00:00Z", nextRunAt: "2026-09-21T01:30:00Z" }] },
      { targetId: "collector-1", kind: "collector", displayName: "采集端 abc12345", clientIp: "10.0.0.2", status: "offline", clientVersion: "0.1.0", includeMedia: true, dataRedaction: true, offlineExportEnabled: false, lastSeenAt: "2026-09-21T02:00:00Z", plans: [{ id: "collector-1", name: "销售电脑", includeMedia: true, schedule: { mode: "daily", intervalMinutes: 60, dailyTime: "03:00" }, createdAt: "2026-09-21T01:00:00Z", updatedAt: "2026-09-21T01:00:00Z", nextRunAt: "2026-09-22T03:00:00Z" }] },
    ],
  };
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); const method = init?.method || "GET"; const body = init?.body ? JSON.parse(String(init.body)) as Record<string, unknown> : undefined; requests.push({ method, url, body });
    if (url.endsWith("/api/v1/enterprise/config")) return Response.json({ organizationName: "测试服务端", uploadUrl: "http://127.0.0.1:9812", keyId: "test-key", includeMedia: false, dataRedaction: true, offlineExportEnabled: false });
    if (method === "PUT") targets = { ...targets, targets: targets.targets.map((target) => target.kind === "server" ? { ...target, plans: [{ ...target.plans[0], name: String(body?.name), includeMedia: Boolean(body?.includeMedia), schedule: body?.schedule as typeof target.plans[0]["schedule"] }] } : target) };
    return Response.json(targets);
  }));
  render(<CollectionSchedulePage token="test-token" />);
  expect((await screen.findAllByText("10.0.0.2")).length).toBeGreaterThan(0);
  expect(screen.getByText(/IP：10\.0\.0\.2/)).toBeInTheDocument();
  expect(screen.getByText(/全内容.*脱敏.*不支持离线导出/)).toBeInTheDocument();
  expect(screen.getByText(/采集计划：每日 03:00 执行/)).toBeInTheDocument();
  expect(screen.getAllByText(/全内容.*脱敏/, { selector: ".collector-target-summary small" })).toHaveLength(2);
  expect(screen.getByText(/采集计划：每 30 分钟执行/)).toBeInTheDocument();
  expect(screen.queryByText(/配置 \d+\/\d+/)).not.toBeInTheDocument();
  expect(screen.getByText("本机", { selector: "strong" })).toBeInTheDocument();
  expect(screen.queryByText("不含图片和文件")).not.toBeInTheDocument();
  expect(screen.queryByText("已启用脱敏")).not.toBeInTheDocument();
  expect(screen.queryByText("不支持离线导出")).not.toBeInTheDocument();
  expect(screen.queryByRole("tab", { name: "采集端" })).not.toBeInTheDocument();
  expect(screen.getByRole("switch", { name: "编辑模式" })).not.toBeChecked();
  fireEvent.click(screen.getByRole("switch", { name: "编辑模式" }));
  expect(screen.queryAllByRole("button", { name: "编辑" })).toHaveLength(0);
  expect(screen.getByText("本机", { selector: "strong" })).toBeInTheDocument();
  expect(screen.getAllByText("采集计划", { selector: ".collector-inline-plan > span" })).toHaveLength(2);
  expect(screen.getByRole("checkbox", { name: "含文件和图片：本机" })).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "数据脱敏：本机" })).toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "支持离线导出：本机" })).not.toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "隐藏式采集端：本机" })).not.toBeInTheDocument();
  expect(screen.queryByText("启用采集")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "添加采集计划" })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("间隔（分钟）"), { target: { value: "45" } });
  fireEvent.click(screen.getByRole("button", { name: "保存" }));
  await waitFor(() => expect(requests).toContainEqual(expect.objectContaining({ method: "PUT", url: expect.stringContaining("/plan-1") })));
  expect(requests.find((request) => request.url.endsWith("/plan-1"))?.body?.schedule).toMatchObject({ intervalMinutes: 45 });
});

it("keeps the collector node visible when it has no plans", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ targets: [{ targetId: "server", kind: "server", displayName: "测试服务端", status: "online", plans: [] }] })));
  render(<CollectionSchedulePage token="test-token" />);
  expect(await screen.findByText("本机")).toBeInTheDocument();
  expect(screen.queryByText("暂无采集计划")).not.toBeInTheDocument();
  expect(screen.queryByRole("article")).not.toBeInTheDocument();
});

it("keeps a plan-less local target disabled when editing its current value", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ targets: [{ targetId: "server", kind: "server", displayName: "测试服务端", status: "online", plans: [] }] })));
  render(<CollectionSchedulePage token="test-token" />);
  await screen.findByText("本机");
  fireEvent.click(screen.getByRole("switch", { name: "编辑模式" }));
  expect(screen.getByLabelText("按间隔")).not.toBeChecked();
  expect(screen.getByLabelText("每日定时")).not.toBeChecked();
  expect(screen.queryByLabelText("间隔（分钟）")).not.toBeInTheDocument();
});

it("automatically discovers new collectors without replacing an active edit", async () => {
  let requestCount = 0;
  let poll: (() => Promise<void>) | undefined;
  vi.spyOn(window, "setInterval").mockImplementation((callback, delay) => {
    if (delay === 5000) poll = callback as () => Promise<void>;
    return 1;
  });
  vi.stubGlobal("fetch", vi.fn(async () => {
    requestCount += 1;
    return Response.json({ targets: [
      { targetId: "server", kind: "server", displayName: "测试服务端", status: "online", plans: [] },
      ...(requestCount > 1 ? [{ targetId: "collector-new", kind: "collector", displayName: "新采集端", status: "online", plans: [] }] : []),
    ] });
  }));
  render(<CollectionSchedulePage token="test-token" />);
  expect(await screen.findByText("本机")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /刷新/ })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("switch", { name: "编辑模式" }));
  expect(screen.queryByRole("switch", { name: "编辑模式" })).not.toBeInTheDocument();
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
    await poll?.();
    await new Promise((resolve) => setTimeout(resolve, 0));
    if (requestCount === 1) await poll?.();
  });
  expect(await screen.findByDisplayValue("新采集端")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "含文件和图片：本机" })).toBeInTheDocument();
  expect(screen.queryByText("自动更新 · 每 5 秒")).not.toBeInTheDocument();
  expect(screen.queryByText("检测到采集端列表更新")).not.toBeInTheDocument();
  expect(requestCount).toBe(2);
});

it("shows the remote plan on three rows and aligns both editors with shared controls", async () => {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/api/v1/collectors/collector-1")) return Response.json({ config: { displayName: "10.0.0.2", enabled: true, includeMedia: true, dataRedaction: true, offlineExportEnabled: false, schedule: { mode: "interval", intervalMinutes: 20, dailyTime: "02:00" } } });
    return Response.json({ targets: [{ targetId: "collector-1", kind: "collector", displayName: "10.0.0.2", clientIp: "10.0.0.2", status: "online", configRevision: 1, lastAppliedRevision: 1, logDirectory: "collector/collector-1", plans: [{ id: "collector-1", name: "远程计划", includeMedia: true, schedule: { mode: "interval", intervalMinutes: 20, dailyTime: "02:00" }, lastRunAt: "2026-09-24T10:00:00Z", nextRunAt: "2026-09-24T10:20:00Z" }] }] });
  }));
  const { container } = render(<CollectionSchedulePage token="test-token" />);
  expect(await screen.findByText(/采集计划：每 20 分钟执行/)).toBeInTheDocument();
  expect(screen.getByText(/服务端日志目录：collector\/collector-1\/日期.log/)).toBeInTheDocument();
  fireEvent.change(screen.getByRole("textbox", { name: "检索采集端" }), { target: { value: "collector-1" } });
  expect(screen.getByText(/实例标识：collector-1/)).toBeInTheDocument();
  const summary = container.querySelector(".collector-target-summary");
  expect(summary?.children).toHaveLength(3);
  expect(summary?.children[1]).toHaveTextContent("最近成功：");
  expect(summary?.children[1]).toHaveTextContent("下次执行：");
  expect(screen.queryByText(/配置 1\/1/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("switch", { name: "编辑模式" }));
  expect(screen.queryByText(/IP：/)).not.toBeInTheDocument();
  expect(screen.queryByRole("dialog", { name: "编辑采集端" })).not.toBeInTheDocument();
  expect(screen.getByText("采集计划", { selector: ".collector-inline-plan > span" })).toBeInTheDocument();
  expect(screen.getByLabelText("间隔（分钟）").parentElement).toHaveTextContent("分钟");
});

it("only shows a successful upload after enabling the plan and ignores collection attempts", async () => {
  const uploadedAt = "2026-09-24T10:00:00Z";
  const attemptedAt = "2026-09-24T09:00:00Z";
  let lastRunAt: string | undefined = attemptedAt;
  let lastUploadAt: string | undefined;
  let schedule: { mode: "disabled" | "interval"; intervalMinutes: number; dailyTime: string } = { mode: "disabled", intervalMinutes: 50, dailyTime: "02:00" };
  let poll: (() => Promise<void>) | undefined;
  vi.spyOn(window, "setInterval").mockImplementation((callback, delay) => {
    if (delay === 5000) poll = callback as () => Promise<void>;
    return 1;
  });
  const fetchTargets = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    if (init?.method === "PUT") {
      schedule = JSON.parse(String(init.body)).schedule as typeof schedule;
      return Response.json({ status: "updated" });
    }
    return Response.json({ targets: [{
    targetId: "collector-first-run", kind: "collector", displayName: "测试采集端", status: "online",
    plans: [{ id: "collector-first-run", name: "测试计划", includeMedia: false,
      schedule,
      lastRunAt, lastUploadAt,
    }],
  }] });
  });
  vi.stubGlobal("fetch", fetchTargets);
  render(<CollectionSchedulePage token="test-token" />);
  expect(await screen.findByText(/最近成功：尚未成功/)).toBeInTheDocument();
  expect(screen.queryByText(/最近执行：/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("switch", { name: "编辑模式" }));
  fireEvent.click(screen.getByLabelText("按间隔"));
  fireEvent.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByText(/采集计划：每 50 分钟执行.*最近成功：尚未成功/)).toBeInTheDocument();
  expect(fetchTargets).toHaveBeenCalledWith(expect.stringContaining("/collectors/collector-first-run"), expect.objectContaining({ method: "PUT", body: expect.stringContaining('"mode":"interval"') }));
  lastUploadAt = uploadedAt;
  await act(async () => { await poll?.(); });
  const uploadTime = new Date(uploadedAt).toLocaleString("zh-CN", { hour12: false });
  expect(await screen.findByText(`采集计划：每 50 分钟执行 最近成功：${uploadTime} 下次执行：尚未执行`, { exact: false })).toBeInTheDocument();

  lastRunAt = "2026-09-24T11:30:00Z";
  await act(async () => { await poll?.(); });
  expect(await screen.findByText(`采集计划：每 50 分钟执行 最近成功：${uploadTime} 下次执行：尚未执行`, { exact: false })).toBeInTheDocument();
});
