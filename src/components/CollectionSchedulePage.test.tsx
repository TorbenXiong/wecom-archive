import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { CollectionSchedulePage } from "./CollectionSchedulePage";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("adds and independently updates local plans while listing generated collector plans", async () => {
  const requests: Array<{ method: string; url: string; body?: Record<string, unknown> }> = [];
  let schedules = {
    localPlans: [{ id: "plan-1", name: "早班采集", includeMedia: false, schedule: { mode: "interval", intervalMinutes: 30, dailyTime: "02:00" }, createdAt: "2026-09-21T01:00:00Z", updatedAt: "2026-09-21T01:00:00Z", nextRunAt: "2026-09-21T01:30:00Z" }],
    collectorPlans: [{ collectorId: "collector-1", fileName: "collector-1.exe", organizationName: "测试企业", uploadUrl: "http://127.0.0.1:9812/api/v1/imports/enterprise", includeMedia: true, dataRedaction: false, offlineExportEnabled: false, schedule: { mode: "daily", intervalMinutes: 60, dailyTime: "03:00" }, createdAt: "2026-09-21T01:00:00Z", nextRunAt: "2026-09-22T03:00:00Z", executableAvailable: true }],
  };
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); const method = init?.method || "GET"; const body = init?.body ? JSON.parse(String(init.body)) as Record<string, unknown> : undefined;
    requests.push({ method, url, body });
    if (method === "POST") schedules = { ...schedules, localPlans: [...schedules.localPlans, { id: "plan-2", name: String(body?.name), includeMedia: Boolean(body?.includeMedia), schedule: body?.schedule as typeof schedules.localPlans[0]["schedule"], createdAt: "2026-09-21T02:00:00Z", updatedAt: "2026-09-21T02:00:00Z", nextRunAt: "2026-09-21T02:30:00Z" }] };
    if (method === "PUT") schedules = { ...schedules, localPlans: schedules.localPlans.map((plan) => url.endsWith(plan.id) ? { ...plan, name: String(body?.name), includeMedia: Boolean(body?.includeMedia), schedule: body?.schedule as typeof plan.schedule } : plan) };
    return Response.json(schedules);
  }));

  render(<CollectionSchedulePage token="test-token" />);
  expect(await screen.findByText("collector-1.exe")).toBeInTheDocument();
  expect(screen.getByText(/预计下次：/)).toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "采集端" })).toHaveAttribute("aria-selected", "true");
  expect(screen.queryByDisplayValue("早班采集")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "添加本机计划" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "采集计划" })).not.toBeInTheDocument();
  expect(screen.queryByText("维护多条本机采集计划，并查看已生成采集端携带的持续采集计划。")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /保存/ })).not.toBeInTheDocument();

  fireEvent.click(screen.getByRole("tab", { name: "本机" }));
  expect(await screen.findByDisplayValue("早班采集")).toBeInTheDocument();
  expect(screen.getByText(/预计下次：/)).toBeInTheDocument();
  expect(screen.queryByText("collector-1.exe")).not.toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "本机" })).toHaveAttribute("aria-selected", "true");

  fireEvent.click(screen.getByRole("button", { name: "添加本机计划" }));
  const addDialog = screen.getByRole("dialog", { name: "添加本机" });
  expect(addDialog.querySelector(".schedule-add-heading")).toContainElement(screen.getByRole("heading", { name: "添加本机" }));
  expect(screen.queryByRole("button", { name: "取消" })).not.toBeInTheDocument();
  expect(screen.queryByText("触发方式")).not.toBeInTheDocument();
  const newPlanInterval = addDialog.querySelector<HTMLInputElement>("#new-local-plan-mode-interval");
  expect(newPlanInterval).not.toBeNull();
  fireEvent.click(newPlanInterval!);
  expect(newPlanInterval).toBeChecked();
  fireEvent.change(screen.getByLabelText("计划名称"), { target: { value: "晚班采集" } });
  fireEvent.click(screen.getByRole("button", { name: "添加计划" }));
  await waitFor(() => expect(requests).toContainEqual(expect.objectContaining({ method: "POST", body: expect.objectContaining({ name: "晚班采集" }) })));
  expect(await screen.findByDisplayValue("晚班采集")).toBeInTheDocument();

  const firstPlanName = screen.getByDisplayValue("早班采集");
  fireEvent.change(firstPlanName, { target: { value: "晨班采集" } });
  fireEvent.blur(firstPlanName);
  await waitFor(() => expect(requests).toContainEqual(expect.objectContaining({ method: "PUT", url: expect.stringContaining("/plan-1"), body: expect.objectContaining({ name: "晨班采集" }) })));

  fireEvent.click(screen.getByLabelText("每日定时", { selector: "#local-plan-plan-1-mode-daily" }));
  await waitFor(() => expect(requests).toContainEqual(expect.objectContaining({ method: "PUT", url: expect.stringContaining("/plan-1"), body: expect.objectContaining({ schedule: expect.objectContaining({ mode: "daily" }) }) })));

  fireEvent.click(screen.getByRole("tab", { name: "采集端" }));
  expect(screen.getByText("collector-1.exe")).toBeInTheDocument();
  expect(screen.queryByDisplayValue("晨班采集")).not.toBeInTheDocument();
});
