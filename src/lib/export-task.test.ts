import { createElement } from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { ExportDialog } from "../components/Overlays";
import { createServerExport, ExportStatusUnknownError, ServerApiError } from "./server-api";

const result = { exportId: "synthetic-export", fileName: "synthetic.json", messageCount: 1 };
const exportRequest = { scope: "entire_archive" as const, format: "json" as const };

it("treats malformed status replies as unknown after the retry budget", async () => {
  vi.useFakeTimers();
  let polls = 0;
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/start")) return Response.json({ taskId: "synthetic-task" });
    polls += 1;
    return Response.json({ status: "completed" });
  }));
  const pending = createServerExport("synthetic-token", exportRequest);
  const assertion = expect(pending).rejects.toMatchObject({ code: "EXPORT_STATUS_UNKNOWN", taskId: "synthetic-task" });
  await vi.runAllTimersAsync();
  await assertion;
  expect(polls).toBe(3);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("retains a client-generated identity after the export start response is lost and resumes without another start", async () => {
  let requestedId = "";
  const mock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    if (String(input).endsWith("/start")) {
      requestedId = new Headers(init?.headers).get("X-Archive-Task-Id") || "";
      throw new TypeError("synthetic start response lost");
    }
    expect(String(input)).toBe(`/api/v1/exports/${requestedId}`);
    return Response.json({ status: "completed", result });
  });
  vi.stubGlobal("fetch", mock);
  let unknown: unknown;
  await expect(createServerExport("synthetic-token", exportRequest).catch((error: unknown) => {
    unknown = error; throw error;
  })).rejects.toBeInstanceOf(ExportStatusUnknownError);
  expect(requestedId).toMatch(/^[0-9a-f-]{36}$/i);
  expect(unknown).toMatchObject({ taskId: requestedId, cancellationRequested: false });
  await expect(createServerExport("synthetic-token", exportRequest, undefined, undefined,
    (unknown as ExportStatusUnknownError).taskId)).resolves.toEqual(result);
  expect(mock.mock.calls.filter(([url]) => String(url).endsWith("/start"))).toHaveLength(1);
});

it("treats a duplicate export start identity as an existing task and preserves a definite business refusal", async () => {
  for (const code of ["TASK_ALREADY_EXISTS", "SUPER_ADMIN_REQUIRED"]) {
    let requestedId = "";
    const mock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      requestedId = new Headers(init?.headers).get("X-Archive-Task-Id") || "";
      return Response.json({ code, message: "合成拒绝" }, { status: code === "TASK_ALREADY_EXISTS" ? 409 : 403 });
    });
    vi.stubGlobal("fetch", mock);
    await expect(createServerExport("synthetic-token", exportRequest)).rejects.toMatchObject(code === "TASK_ALREADY_EXISTS"
      ? { code: "EXPORT_STATUS_UNKNOWN", taskId: requestedId }
      : { code: "SUPER_ADMIN_REQUIRED", status: 403 });
    expect(mock).toHaveBeenCalledTimes(1);
  }
});

it("returns a capacity refusal without querying or falling back to another export POST", async () => {
  const mock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => Response.json({
    code: "EXPORT_TASK_CAPACITY_REACHED", message: "合成导出记录容量已满",
  }, { status: 503 }));
  vi.stubGlobal("fetch", mock);

  const refusal: unknown = await createServerExport("synthetic-token", exportRequest).catch((error: unknown) => error);
  expect(refusal).toBeInstanceOf(ServerApiError);
  expect(refusal).not.toBeInstanceOf(ExportStatusUnknownError);
  expect(refusal).toMatchObject({ status: 503, code: "EXPORT_TASK_CAPACITY_REACHED" });
  expect(mock).toHaveBeenCalledOnce();
  expect(mock.mock.calls[0][0]).toBe("/api/v1/exports/start");
  expect(mock.mock.calls[0][1]).toMatchObject({ method: "POST" });
});

it("keeps coded uncertain start failures unknown and confirms the original ID with GET only", async () => {
  for (const [status, code] of [
    [503, "UPSTREAM_TIMEOUT"], [500, "ARCHIVE_STORE_FAILED"],
    [408, "REQUEST_TIMEOUT"], [409, "UPSTREAM_CONFLICT"], [429, "TOO_MANY_REQUESTS"],
  ] as const) {
    let requestedId = "";
    const mock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith("/start")) {
        requestedId = new Headers(init?.headers).get("X-Archive-Task-Id") || "";
        return Response.json({ code, message: "合成响应不能确认任务结果" }, { status });
      }
      expect(String(input)).toBe(`/api/v1/exports/${requestedId}`);
      expect(init?.method ?? "GET").toBe("GET");
      return Response.json({ status: "completed", result });
    });
    vi.stubGlobal("fetch", mock);

    const unknown: unknown = await createServerExport("synthetic-token", exportRequest).catch((error: unknown) => error);
    expect(requestedId).toMatch(/^[0-9a-f-]{36}$/i);
    expect(unknown).toBeInstanceOf(ExportStatusUnknownError);
    expect(unknown).toMatchObject({ taskId: requestedId, cancellationRequested: false });
    expect(mock).toHaveBeenCalledOnce();
    await expect(createServerExport("synthetic-token", exportRequest, undefined, undefined, requestedId)).resolves.toEqual(result);
    expect(mock.mock.calls.map(([url]) => String(url))).toEqual([
      "/api/v1/exports/start", `/api/v1/exports/${requestedId}`,
    ]);
  }
});

it("keeps expired or missing export queries unknown without submitting another export", async () => {
  const taskId = crypto.randomUUID();
  for (const [status, code] of [[400, "INVALID_QUERY"], [404, "TASK_NOT_FOUND"]] as const) {
    const mock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      expect(String(input)).toBe(`/api/v1/exports/${taskId}`);
      expect(init?.method ?? "GET").toBe("GET");
      return Response.json({ code, message: "合成旧任务已不可查询" }, { status });
    });
    vi.stubGlobal("fetch", mock);

    await expect(createServerExport("synthetic-token", exportRequest, undefined, undefined, taskId)).rejects.toMatchObject({
      code: "EXPORT_STATUS_UNKNOWN", taskId, cancellationRequested: false,
    });
    expect(mock).toHaveBeenCalledOnce();
  }
});

it("keeps the generated export identity when the acknowledgement lacks a task ID", async () => {
  let requestedId = "";
  vi.stubGlobal("fetch", vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    requestedId = new Headers(init?.headers).get("X-Archive-Task-Id") || "";
    return Response.json({});
  }));
  const pending = createServerExport("synthetic-token", exportRequest);
  await expect(pending).rejects.toMatchObject({ code: "EXPORT_STATUS_UNKNOWN", taskId: requestedId });
});

it("preserves cancellation intent when the export start response disappears", async () => {
  const controller = new AbortController();
  let requestedId = "";
  vi.stubGlobal("fetch", vi.fn(async (_input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    requestedId = new Headers(init?.headers).get("X-Archive-Task-Id") || "";
    controller.abort();
    throw new TypeError("synthetic start response lost");
  }));
  await expect(createServerExport("synthetic-token", exportRequest, controller.signal)).rejects.toMatchObject({
    code: "EXPORT_STATUS_UNKNOWN", taskId: requestedId, cancellationRequested: true,
  });
});

it("retains the legacy synchronous export fallback when start is unavailable", async () => {
  const mock = vi.fn(async (input: RequestInfo | URL) => String(input).endsWith("/start")
    ? Response.json({}, { status: 404 }) : Response.json(result));
  vi.stubGlobal("fetch", mock);
  await expect(createServerExport("synthetic-token", exportRequest)).resolves.toEqual(result);
  expect(mock.mock.calls.map(([url]) => String(url))).toEqual(["/api/v1/exports/start", "/api/v1/exports"]);
});

it("retries transient export status failures within a three-attempt budget", async () => {
  vi.useFakeTimers();
  let polls = 0;
  const mock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    expect(init?.signal).toBeInstanceOf(AbortSignal);
    if (String(input).endsWith("/start")) return Response.json({ taskId: "synthetic-task" });
    polls += 1;
    if (polls === 1) throw new TypeError("synthetic connection interruption");
    if (polls === 2) return Response.json({ message: "synthetic unavailable" }, { status: 503 });
    return Response.json({ status: "completed", result });
  });
  vi.stubGlobal("fetch", mock);
  const pending = createServerExport("synthetic-token", exportRequest);
  const assertion = expect(pending).resolves.toEqual(result);
  await vi.runAllTimersAsync();
  await assertion;
  expect(polls).toBe(3);
  expect(mock.mock.calls.filter(([url]) => String(url).endsWith("/start"))).toHaveLength(1);
});

it("retains the original task after exhausted status retries and resumes without another start", async () => {
  vi.useFakeTimers();
  let available = false;
  let polls = 0;
  const mock = vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/start")) return Response.json({ taskId: "synthetic-task" });
    polls += 1;
    if (!available) throw new TypeError("synthetic connection interruption");
    return Response.json({ status: "completed", result });
  });
  vi.stubGlobal("fetch", mock);
  const pending = createServerExport("synthetic-token", exportRequest);
  const assertion = expect(pending).rejects.toMatchObject({
    code: "EXPORT_STATUS_UNKNOWN", taskId: "synthetic-task", cancellationRequested: false,
  });
  await vi.runAllTimersAsync();
  await assertion;
  expect(polls).toBe(3);
  available = true;
  await expect(createServerExport("synthetic-token", exportRequest, undefined, undefined, "synthetic-task")).resolves.toEqual(result);
  expect(mock.mock.calls.filter(([url]) => String(url).endsWith("/start"))).toHaveLength(1);
});

it("safely retries a lost cancellation response and reports cancellation only when confirmed", async () => {
  vi.useFakeTimers();
  const controller = new AbortController();
  const statuses: string[] = [];
  let cancellations = 0;
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.endsWith("/start")) { controller.abort(); return Response.json({ taskId: "synthetic-task" }); }
    if (url.endsWith("/cancel")) {
      cancellations += 1;
      if (cancellations === 1) throw new TypeError("synthetic cancellation response lost");
      return Response.json({ status: "cancelling", stage: "清理中", percent: 20 });
    }
    return Response.json({ status: "cancelled", stage: "已清理", percent: 20 });
  }));
  const pending = createServerExport("synthetic-token", exportRequest, controller.signal, (progress) => statuses.push(progress.status));
  const assertion = expect(pending).rejects.toMatchObject({ code: "EXPORT_CANCELLED" });
  await vi.runAllTimersAsync();
  await assertion;
  expect(cancellations).toBe(2);
  expect(statuses).toEqual(["cancelling", "cancelled"]);
});

it("keeps cancellation intent when all cancellation responses are lost", async () => {
  vi.useFakeTimers();
  const controller = new AbortController();
  const statuses: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/start")) { controller.abort(); return Response.json({ taskId: "synthetic-task" }); }
    throw new TypeError("synthetic cancellation response lost");
  }));
  const pending = createServerExport("synthetic-token", exportRequest, controller.signal, (progress) => statuses.push(progress.status));
  const assertion = expect(pending).rejects.toMatchObject({
    code: "EXPORT_STATUS_UNKNOWN", taskId: "synthetic-task", cancellationRequested: true,
  });
  await vi.runAllTimersAsync();
  await assertion;
  expect(statuses).toEqual([]);
});

it("returns an actual terminal export failure without retrying it as a status request failure", async () => {
  const mock = vi.fn(async (input: RequestInfo | URL) => String(input).endsWith("/start")
    ? Response.json({ taskId: "synthetic-task" })
    : Response.json({ status: "failed", error: "合成导出写入失败", errorCode: "ARCHIVE_EXPORT_FAILED" }));
  vi.stubGlobal("fetch", mock);
  await expect(createServerExport("synthetic-token", exportRequest)).rejects.toMatchObject({ code: "ARCHIVE_EXPORT_FAILED" });
  expect(mock).toHaveBeenCalledTimes(2);
});

it("retains a cancellation requested while a status query is retrying", async () => {
  vi.useFakeTimers();
  const controller = new AbortController();
  let polls = 0;
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/start")) return Response.json({ taskId: "synthetic-task" });
    polls += 1;
    if (polls === 2) controller.abort();
    throw new TypeError("synthetic status response lost");
  }));
  const pending = createServerExport("synthetic-token", exportRequest, controller.signal);
  const assertion = expect(pending).rejects.toMatchObject({
    code: "EXPORT_STATUS_UNKNOWN", taskId: "synthetic-task", cancellationRequested: true,
  });
  await vi.runAllTimersAsync();
  await assertion;
});

it("continues the original export from the dialog after an unknown result", async () => {
  const onConfirm = vi.fn()
    .mockRejectedValueOnce(new ExportStatusUnknownError("synthetic-task", true))
    .mockResolvedValueOnce(undefined);
  render(createElement(ExportDialog, {
    token: "synthetic-token", scope: "entire_archive", format: "json",
    dataRedaction: true, simplify: true, pretty: true, conversationOrder: "descending", messageOrder: "descending",
    onConversationOrderChange: vi.fn(), onMessageOrderChange: vi.fn(), onSimplifyChange: vi.fn(), onPrettyChange: vi.fn(),
    messageCount: 1, mediaCount: 0, conversations: [], selectedConversationId: "", onScopeChange: vi.fn(), onFormatChange: vi.fn(),
    onDataRedactionChange: vi.fn(), onOpenDirectoryFailed: vi.fn(), onClose: vi.fn(), onConfirm,
  }));
  fireEvent.click(screen.getByRole("button", { name: "开始导出" }));
  const resume = await screen.findByRole("button", { name: "继续查询" });
  expect(screen.getByText(/原任务可能仍在运行/)).toBeInTheDocument();
  expect(screen.getByRole("radio", { name: "全部档案" })).toBeDisabled();
  fireEvent.click(resume);
  await waitFor(() => expect(onConfirm).toHaveBeenCalledTimes(2));
  expect(onConfirm.mock.calls[0]).toHaveLength(3);
  expect(onConfirm.mock.calls[1][3]).toBe("synthetic-task");
  expect((onConfirm.mock.calls[1][1] as AbortSignal).aborted).toBe(true);
});
