import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { ArchiveHealthPage } from "./ArchiveHealthPage";
import { TaskCenterPage } from "./TaskCenterPage";
import type { ArchiveHealth, TaskHistoryRecord } from "../lib/server-api";

afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const record: TaskHistoryRecord = { taskId: "synthetic-id", kind: "import", status: "unknown",
  startedAt: "2026-01-02T03:04:05Z", updatedAt: "2026-01-02T03:04:05Z", receivedBytes: 0, phase: "写入归档并提交事务" };
const health: ArchiveHealth = { revision: 2, messageCount: 10, conversationCount: 2, sourceCount: 1, batchCount: 3,
  unsupportedMessages: 1, verifiedMedia: 5, missingMedia: 2, invalidMedia: 0, restrictedBatches: 1,
  missingLocator: 1, missingInSource: 1, missingOther: 0, checkedAt: "2026-01-02T03:04:05Z",
  latestVerification: { ...record, kind: "health_check", status: "completed", verification: {
    revision: 1, databaseOk: true, checkedMedia: 5, missingFiles: 1, damagedFiles: 1, unreadableFiles: 0,
  } } };

it("separates source media status from archive file damage and flags a stale verification", async () => {
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer synthetic-token");
    return Response.json(health);
  });
  vi.stubGlobal("fetch", fetchMock);
  const open = vi.fn();
  render(<ArchiveHealthPage token="synthetic-token" onOpenTasks={open} />);
  await screen.findByRole("heading", { name: "发现 2 项附件异常" });
  expect(screen.getByText("采集时缺失")).toBeVisible();
  expect(screen.getByText(/归档已有更新/)).toBeVisible();
  expect(screen.getByText("数据库完整性检查通过")).not.toBeVisible();
  fireEvent.click(screen.getByText("校验详情"));
  expect(screen.getByText("数据库完整性检查通过")).toBeVisible();
  expect(screen.getByText("归档文件缺失")).toBeVisible();
  expect(fetchMock).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole("button", { name: "查看任务中心" }));
  expect(open).toHaveBeenCalledTimes(1);
});

it("starts verification only on demand and preserves uncertain registration feedback", async () => {
  let posts = 0;
  vi.stubGlobal("fetch", vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    if (init?.method === "POST") { posts += 1; throw new TypeError("合成连接中断"); }
    return Response.json({ ...health, latestVerification: null });
  }));
  render(<ArchiveHealthPage token="synthetic-token" onOpenTasks={vi.fn()} />);
  await screen.findByRole("heading", { name: "尚未执行完整校验" });
  expect(posts).toBe(0);
  fireEvent.click(screen.getByRole("button", { name: "开始完整校验" }));
  await screen.findByRole("alert");
  expect(screen.getByRole("alert")).toHaveTextContent("请先在任务中心核对");
  expect(posts).toBe(1);
});

it("filters and pages durable records, warns on storage failure, and never reruns unknown work", async () => {
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    expect(init?.method).not.toBe("POST");
    const url = new URL(String(input), "http://localhost");
    return Response.json({ records: [record], total: url.searchParams.get("kind") ? 1 : 51, persistenceError: true });
  });
  vi.stubGlobal("fetch", fetchMock);
  render(<TaskCenterPage token="synthetic-token" superAdminEnabled />);
  await screen.findByText("需核对归档或导出文件");
  expect(screen.queryByText(record.taskId)).not.toBeInTheDocument();
  expect(screen.queryByText(record.phase!)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "展开任务详情" }));
  expect(screen.getByText(record.taskId)).toBeVisible();
  expect(screen.getByText(record.phase!)).toBeVisible();
  expect(screen.getByRole("alert")).toHaveTextContent("任务记录保存失败");
  fireEvent.click(screen.getByRole("button", { name: "刷新任务" }));
  await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
  expect(screen.getByText(record.taskId)).toBeVisible();
  expect(screen.queryByRole("button", { name: /重试|重新执行/ })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "下一页" }));
  await waitFor(() => expect(String(fetchMock.mock.calls.at(-1)?.[0])).toContain("offset=50"));
  fireEvent.change(screen.getByLabelText("任务类型"), { target: { value: "import" } });
  await waitFor(() => expect(String(fetchMock.mock.calls.at(-1)?.[0])).toContain("kind=import"));
  expect(String(fetchMock.mock.calls.at(-1)?.[0])).toContain("offset=0");
});

it("keeps low-frequency help collapsed while leaving exception counts visible", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json(health)));
  render(<ArchiveHealthPage token="synthetic-token" onOpenTasks={vi.fn()} />);
  await screen.findByRole("heading", { name: "发现 2 项附件异常" });
  const scope = screen.getByText(/仅检查已归档内容及已采集附件/);
  expect(scope).not.toBeVisible();
  fireEvent.click(screen.getByText("检查范围"));
  expect(scope).toBeVisible();
  expect(screen.getByText("未解析到附件位置")).not.toBeVisible();
  fireEvent.click(screen.getByText("查看缺失原因"));
  expect(screen.getByText("未解析到附件位置")).toBeVisible();
});

it("keeps completed tasks concise and exposes their output only when expanded", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ records: [{ ...record, kind: "export", status: "completed", output: {
    fileName: "synthetic.json", exportId: "synthetic-export", messageCount: 10, mediaCount: 2, missingMediaCount: 0, sha256: "a".repeat(64),
  } }], total: 1, persistenceError: false })));
  render(<TaskCenterPage token="synthetic-token" superAdminEnabled />);
  await screen.findByText("已导出 10 条消息");
  expect(screen.getByText("已完成", { selector: ".task-state" })).toBeVisible();
  expect(screen.queryByText("synthetic.json")).not.toBeInTheDocument();
  expect(screen.queryByText(record.taskId)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "打开导出目录" })).toBeEnabled();
  fireEvent.click(screen.getByRole("button", { name: "展开任务详情" }));
  expect(screen.getByText("synthetic.json")).toBeVisible();
  expect(screen.getByRole("button", { name: "收起任务详情" })).toHaveAttribute("aria-expanded", "true");
});
