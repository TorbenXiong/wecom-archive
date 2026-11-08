import { afterEach, vi } from "vitest";
import {
  collectLocalArchive, importClientJson, importEnterprisePackage, resumeArchiveTaskStatus,
  ServerApiError, type ServerImportResult,
} from "./server-api";
import { ArchiveTaskPendingError, type ArchiveTaskKind, type ArchiveTaskProgress } from "./long-task";

const token = "synthetic-archive-request-token";
const result: ServerImportResult = {
  exportId: "synthetic-export", batchCount: 1, inserted: 1, unchanged: 0, revised: 0, mediaCount: 0, missingMediaCount: 0,
};

function progress(taskId: string, kind: ArchiveTaskKind, status: ArchiveTaskProgress["status"], extra: Partial<ArchiveTaskProgress> = {}): ArchiveTaskProgress {
  return {
    taskId, kind, status, stage: "合成任务阶段", receivedBytes: 0,
    startedAt: "2026-01-02T03:04:05Z", updatedAt: "2026-01-02T03:04:05Z", ...extra,
  };
}

const operations = ["json", "enterprise", "collection"] as const;
type OperationName = typeof operations[number];
interface Operation {
  kind: ArchiveTaskKind;
  path: string;
  body?: File;
  contentType?: string;
  start: () => Promise<ServerImportResult>;
}

function operation(name: OperationName): Operation {
  if (name === "collection") return {
    kind: "collection", path: "/api/v1/collections/local?include_media=true&data_redaction=true",
    start: () => collectLocalArchive(token, true, undefined, true),
  };
  const contentType = name === "json" ? "application/json" : "application/octet-stream";
  const body = new File(["{}"], name === "json" ? "synthetic-import.json" : "synthetic-package.wca", { type: contentType });
  return {
    kind: "import", path: name === "json" ? "/api/v1/imports/json" : "/api/v1/imports/enterprise", body, contentType,
    start: () => name === "json" ? importClientJson(body, token) : importEnterprisePackage(body, token),
  };
}

function expectPost(input: RequestInfo | URL, init: RequestInit | undefined, source: Operation): string {
  expect(String(input)).toBe(source.path);
  expect(init?.method).toBe("POST");
  expect(init?.body).toBe(source.body);
  const headers = new Headers(init?.headers);
  expect(headers.get("Authorization")).toBe(`Bearer ${token}`);
  expect(headers.get("Content-Type")).toBe(source.contentType ?? null);
  const id = headers.get("X-Archive-Task-Id") || "";
  expect(id).toMatch(/^[0-9a-f-]{36}$/i);
  return id;
}

function expectGet(input: RequestInfo | URL, init: RequestInit | undefined, id: string): void {
  expect(String(input)).toBe(`/api/v1/tasks/${id}`);
  expect(init?.method ?? "GET").toBe("GET");
  expect(init?.body).toBeUndefined();
  expect(new Headers(init?.headers).get("Authorization")).toBe(`Bearer ${token}`);
}

function rejection(status: number, code?: string): Response {
  return Response.json(code ? { code, message: "合成任务响应" } : {}, { status });
}

function settle<T>(pending: Promise<T>): Promise<{ value: T } | { error: unknown }> {
  return pending.then((value) => ({ value }), (error: unknown) => ({ error }));
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("recovers a coded gateway refusal by querying the original import without resending its body", async () => {
  vi.useFakeTimers();
  let taskId = "";
  const file = new File(["{}"], "synthetic-import.json", { type: "application/json" });
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const headers = new Headers(init?.headers);
    expect(headers.get("Authorization")).toBe(`Bearer ${token}`);
    if (init?.method === "POST") {
      expect(String(input)).toBe("/api/v1/imports/json");
      taskId = headers.get("X-Archive-Task-Id") || "";
      expect(init.body).toBe(file);
      return Response.json({ code: "UPSTREAM_TIMEOUT", message: "合成网关响应不代表任务失败" }, { status: 503 });
    }
    expect(String(input)).toBe(`/api/v1/tasks/${taskId}`);
    expect(init?.method ?? "GET").toBe("GET");
    expect(init?.body).toBeUndefined();
    return Response.json(progress(taskId, "import", "completed", { result }));
  });
  vi.stubGlobal("fetch", fetchMock);

  const pending = importClientJson(file, token).then((value) => ({ value }), (error: unknown) => ({ error }));
  await vi.runAllTimersAsync();
  expect(await pending).toEqual({ value: result });
  expect(taskId).toMatch(/^[0-9a-f-]{36}$/i);
  expect(fetchMock.mock.calls.map(([input, init]) => [String(input), init?.method ?? "GET"])).toEqual([
    ["/api/v1/imports/json", "POST"], [`/api/v1/tasks/${taskId}`, "GET"],
  ]);
  expect(vi.getTimerCount()).toBe(0);
});

it("preserves each public operation's body and identity across uncertain POST responses", async () => {
  vi.useFakeTimers();
  const responses: Array<{ status: number; code?: string; malformed?: boolean }> = [
    { status: 500, code: "ARCHIVE_STORE_FAILED" }, { status: 502, code: "BAD_GATEWAY" },
    { status: 503, code: "UPSTREAM_TIMEOUT" }, { status: 504, code: "GATEWAY_TIMEOUT" },
    { status: 500 }, { status: 502 }, { status: 503 }, { status: 504 },
    { status: 502, malformed: true }, { status: 408, code: "UPSTREAM_TIMEOUT" },
    { status: 409, code: "UPSTREAM_CONFLICT" }, { status: 429, code: "RATE_LIMITED" },
  ];
  for (const name of operations) {
    for (const reply of responses) {
      const source = operation(name);
      let id = "";
      const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === "POST") {
          id = expectPost(input, init, source);
          return reply.malformed ? new Response("synthetic non-JSON gateway body", { status: reply.status }) : rejection(reply.status, reply.code);
        }
        expectGet(input, init, id);
        return Response.json(progress(id, source.kind, "completed", { result }));
      });
      vi.stubGlobal("fetch", fetchMock);
      const pending = settle(source.start());
      await vi.runAllTimersAsync();
      expect(await pending).toEqual({ value: result });
      expect(fetchMock.mock.calls).toHaveLength(2);
      expect(vi.getTimerCount()).toBe(0);
    }
  }
});

it("preserves explicit registration refusals and known business failures without a status endpoint", async () => {
  vi.useFakeTimers();
  const refusals = [
    [503, "TASK_CAPACITY_REACHED"], [401, "UNAUTHORIZED"], [403, "SUPER_ADMIN_REQUIRED"],
    [400, "INVALID_TASK_ID"], [400, "INVALID_QUERY"], [422, "INVALID_CLIENT_EXPORT"],
    [409, "LOCAL_COLLECTION_RUNNING"], [409, "BATCH_CONFLICT"],
    [503, "IMPORT_BUSY"], [408, "IMPORT_RECEIVE_TIMEOUT"],
  ] as const;
  for (const name of operations) {
    for (const [status, code] of refusals) {
      const source = operation(name);
      const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === "POST") {
          expectPost(input, init, source);
          return rejection(status, code);
        }
        // Older servers need not expose status queries to report a definite
        // busy/validation response. A mistaken poll must fail the assertions.
        return rejection(404, "TASK_NOT_FOUND");
      });
      vi.stubGlobal("fetch", fetchMock);
      const pending = settle(source.start());
      await vi.runAllTimersAsync();
      const outcome = await pending;
      const error = "error" in outcome ? outcome.error : undefined;
      expect(error).toBeInstanceOf(ServerApiError);
      expect(error).not.toBeInstanceOf(ArchiveTaskPendingError);
      expect(error).toMatchObject({ status, code, message: "合成任务响应" });
      expect(fetchMock.mock.calls).toHaveLength(1);
      expect(vi.getTimerCount()).toBe(0);
    }
  }
});

it("requires exact status and code pairs before treating transient statuses as definite failures", async () => {
  vi.useFakeTimers();
  for (const name of operations) {
    for (const [status, code] of [
      [500, "TASK_CAPACITY_REACHED"], [500, "IMPORT_BUSY"], [503, "IMPORT_RECEIVE_TIMEOUT"],
      [503, "LOCAL_COLLECTION_RUNNING"], [500, "BATCH_CONFLICT"], [429, "TASK_CAPACITY_REACHED"],
    ] as const) {
      const source = operation(name);
      let id = "";
      const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === "POST") {
          id = expectPost(input, init, source);
          return rejection(status, code);
        }
        expectGet(input, init, id);
        return Response.json(progress(id, source.kind, "completed", { result }));
      });
      vi.stubGlobal("fetch", fetchMock);
      const pending = settle(source.start());
      await vi.runAllTimersAsync();
      expect(await pending).toEqual({ value: result });
      expect(fetchMock.mock.calls).toHaveLength(2);
      expect(vi.getTimerCount()).toBe(0);
    }
  }
});

it("keeps the original ID after its query budget and resumes only that operation", async () => {
  vi.useFakeTimers();
  for (const name of operations) {
    for (const unavailable of ["network", "missing", "gateway"] as const) {
      const source = operation(name);
      let id = "";
      let resumed = false;
      const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === "POST") {
          id = expectPost(input, init, source);
          return rejection(503, "UPSTREAM_TIMEOUT");
        }
        expectGet(input, init, id);
        if (resumed) return Response.json(progress(id, source.kind, "completed", { result }));
        if (unavailable === "network") throw new TypeError("synthetic status connection lost");
        return rejection(unavailable === "missing" ? 404 : 503, unavailable === "missing" ? "TASK_NOT_FOUND" : "UPSTREAM_TIMEOUT");
      });
      vi.stubGlobal("fetch", fetchMock);
      const pending = settle(source.start());
      await vi.runAllTimersAsync();
      const outcome = await pending;
      const error = "error" in outcome ? outcome.error : undefined;
      expect(error).toBeInstanceOf(ArchiveTaskPendingError);
      expect(error).toMatchObject({ code: "TASK_STATUS_UNKNOWN", taskId: id, kind: source.kind });
      expect(fetchMock.mock.calls).toHaveLength(4);
      expect(vi.getTimerCount()).toBe(0);
      resumed = true;
      await expect(resumeArchiveTaskStatus(token, id, source.kind)).resolves.toEqual(result);
      expect(fetchMock.mock.calls).toHaveLength(5);
      expect(fetchMock.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(1);
      expect(vi.getTimerCount()).toBe(0);
    }
  }
});

it("returns server-confirmed terminal failures with their business error instead of retrying them", async () => {
  vi.useFakeTimers();
  for (const name of operations) {
    for (const [status, code] of [[409, "BATCH_CONFLICT"], [503, "IMPORT_BUSY"], [500, "ARCHIVE_STORE_FAILED"]] as const) {
      const source = operation(name);
      let id = "";
      const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === "POST") {
          id = expectPost(input, init, source);
          return rejection(503, "UPSTREAM_TIMEOUT");
        }
        expectGet(input, init, id);
        return Response.json(progress(id, source.kind, "failed", {
          error: "合成已确认任务失败", errorCode: code, errorStatus: status,
        }));
      });
      vi.stubGlobal("fetch", fetchMock);
      const pending = settle(source.start());
      await vi.runAllTimersAsync();
      const outcome = await pending;
      const error = "error" in outcome ? outcome.error : undefined;
      expect(error).toBeInstanceOf(ServerApiError);
      expect(error).not.toBeInstanceOf(ArchiveTaskPendingError);
      expect(error).toMatchObject({ status, code, message: "合成已确认任务失败" });
      expect(fetchMock.mock.calls).toHaveLength(2);
      expect(vi.getTimerCount()).toBe(0);
    }
  }
});

it("continues a healthy original POST when an older server lacks status queries", async () => {
  vi.useFakeTimers();
  for (const name of operations) {
    const source = operation(name);
    let id = "";
    let resolvePost!: (response: Response) => void;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      if (init?.method === "POST") {
        id = expectPost(input, init, source);
        return new Promise<Response>((resolve) => { resolvePost = resolve; });
      }
      expectGet(input, init, id);
      return rejection(404, "TASK_NOT_FOUND");
    });
    vi.stubGlobal("fetch", fetchMock);
    let settled = false;
    const pending = settle(source.start()).then((outcome) => { settled = true; return outcome; });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(settled).toBe(false);
    expect(fetchMock.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(1);
    expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(4);
    resolvePost(Response.json(result));
    expect(await pending).toEqual({ value: result });
    const requests = fetchMock.mock.calls.length;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(fetchMock.mock.calls).toHaveLength(requests);
    expect(vi.getTimerCount()).toBe(0);
  }
});

it("queries a duplicate registration's original ID without submitting a replacement operation", async () => {
  vi.useFakeTimers();
  for (const name of operations) {
    const source = operation(name);
    let id = "";
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.method === "POST") {
        id = expectPost(input, init, source);
        return rejection(409, "TASK_ALREADY_EXISTS");
      }
      expectGet(input, init, id);
      return Response.json(progress(id, source.kind, "completed", { result }));
    });
    vi.stubGlobal("fetch", fetchMock);
    const pending = settle(source.start());
    await vi.runAllTimersAsync();
    expect(await pending).toEqual({ value: result });
    expect(fetchMock.mock.calls).toHaveLength(2);
    expect(vi.getTimerCount()).toBe(0);
  }
});
