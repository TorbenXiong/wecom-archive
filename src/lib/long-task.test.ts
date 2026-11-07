import { afterEach, vi } from "vitest";
import { ArchiveTaskPendingError, resumeArchiveTask, trackArchiveRequest, type ArchiveTaskProgress } from "./long-task";
import type { ServerImportResult } from "./server-api";

const result: ServerImportResult = {
  exportId: "synthetic-export", batchCount: 1, inserted: 1, unchanged: 0, revised: 0, mediaCount: 0, missingMediaCount: 0,
};

function progress(taskId: string, status: ArchiveTaskProgress["status"], extra: Partial<ArchiveTaskProgress> = {}): ArchiveTaskProgress {
  return {
    taskId, kind: "import", status, stage: "合成阶段", receivedBytes: 0,
    startedAt: "2026-01-02T03:04:05Z", updatedAt: "2026-01-02T03:04:05Z", ...extra,
  };
}

const fail = (value: ArchiveTaskProgress) => Object.assign(new Error(value.error || "合成任务失败"), {
  code: value.errorCode, status: value.errorStatus,
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

it("recovers a lost POST response by querying the same task without resending the operation", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ServerImportResult> => { throw new TypeError("synthetic response lost"); });
  const read = vi.fn(async (id: string, _signal: AbortSignal) => progress(id, "completed", { result }));
  const updates: ArchiveTaskProgress[] = [];
  const pending = trackArchiveRequest("import", send, read, fail, () => false, (value) => updates.push(value));
  const assertion = expect(pending).resolves.toEqual(result);
  await vi.runAllTimersAsync();
  await assertion;
  expect(send).toHaveBeenCalledTimes(1);
  expect(read).toHaveBeenCalledTimes(1);
  const id = send.mock.calls[0][0];
  expect(read.mock.calls[0][0]).toBe(id);
  expect(updates.map((value) => value.taskId)).toEqual([id, id]);
  expect(updates.map((value) => value.status)).toEqual(["queued", "completed"]);
});

it("retries a transient query failure while preserving the original import identity", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ServerImportResult> => { throw new TypeError("synthetic response lost"); });
  let polls = 0;
  const read = vi.fn(async (id: string, _signal: AbortSignal) => {
    polls += 1;
    if (polls === 1) throw new TypeError("synthetic query interruption");
    return progress(id, polls === 2 ? "running" : "completed", polls === 2 ? {} : { result });
  });
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  const assertion = expect(pending).resolves.toEqual(result);
  await vi.runAllTimersAsync();
  await assertion;
  expect(send).toHaveBeenCalledTimes(1);
  expect(read).toHaveBeenCalledTimes(3);
  expect(new Set(read.mock.calls.map(([id]) => id))).toEqual(new Set([send.mock.calls[0][0]]));
});

it("preserves an unknown task ID after its retry budget and resumes only that task", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ServerImportResult> => { throw new TypeError("synthetic response lost"); });
  const read = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ArchiveTaskProgress> => { throw new TypeError("synthetic query unavailable"); });
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  let unknown: unknown;
  const assertion = expect(pending.catch((error: unknown) => { unknown = error; throw error; })).rejects.toBeInstanceOf(ArchiveTaskPendingError);
  await vi.runAllTimersAsync();
  await assertion;
  expect(read).toHaveBeenCalledTimes(3);
  expect(unknown).toMatchObject({ code: "TASK_STATUS_UNKNOWN", taskId: send.mock.calls[0][0], kind: "import" });
  const original = unknown as ArchiveTaskPendingError;
  const resumeRead = vi.fn(async (_signal: AbortSignal) => progress(original.taskId, "completed", { result }));
  await expect(resumeArchiveTask(original.taskId, original.kind, resumeRead, fail)).resolves.toEqual(result);
  expect(send).toHaveBeenCalledTimes(1);
  expect(resumeRead).toHaveBeenCalledTimes(1);
  expect(resumeRead.mock.calls[0][0].aborted).toBe(true);
  expect(vi.getTimerCount()).toBe(0);
});

it("keeps a server-confirmed failure and its business error without retrying the failed task", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ServerImportResult> => { throw new TypeError("synthetic response lost"); });
  const read = vi.fn(async (id: string, _signal: AbortSignal) => progress(id, "failed", {
    error: "合成批次冲突", errorCode: "BATCH_CONFLICT", errorStatus: 409,
  }));
  const failure = vi.fn(fail);
  const pending = trackArchiveRequest("import", send, read, failure, () => false);
  const assertion = expect(pending).rejects.toMatchObject({ message: "合成批次冲突", code: "BATCH_CONFLICT", status: 409 });
  await vi.runAllTimersAsync();
  await assertion;
  expect(send).toHaveBeenCalledTimes(1);
  expect(read).toHaveBeenCalledTimes(1);
  expect(failure).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

it("continues awaiting a healthy original POST when a legacy server lacks the status endpoint", async () => {
  vi.useFakeTimers();
  let resolve!: (value: ServerImportResult) => void;
  const send = vi.fn((_id: string, _signal: AbortSignal) => new Promise<ServerImportResult>((done) => { resolve = done; }));
  const read = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ArchiveTaskProgress> => {
    throw Object.assign(new Error("synthetic legacy status route"), { status: 404 });
  });
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  let settled = false;
  void pending.then(() => { settled = true; }, () => { settled = true; });
  await vi.advanceTimersByTimeAsync(10_000);
  expect(read.mock.calls.length).toBeGreaterThanOrEqual(3);
  expect(settled).toBe(false);
  expect(send).toHaveBeenCalledTimes(1);
  resolve(result);
  await expect(pending).resolves.toEqual(result);
  const polls = read.mock.calls.length;
  await vi.advanceTimersByTimeAsync(10_000);
  expect(read).toHaveBeenCalledTimes(polls);
  expect(vi.getTimerCount()).toBe(0);
});

it("aborts an in-flight status GET when the original POST supplies the terminal result", async () => {
  vi.useFakeTimers();
  let resolve!: (value: ServerImportResult) => void;
  let queryAborted = false;
  const send = vi.fn((_id: string, _signal: AbortSignal) => new Promise<ServerImportResult>((done) => { resolve = done; }));
  const read = vi.fn((_id: string, signal: AbortSignal) => new Promise<ArchiveTaskProgress>((_done, reject) => {
    signal.addEventListener("abort", () => { queryAborted = true; reject(signal.reason); }, { once: true });
  }));
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  await vi.advanceTimersByTimeAsync(500);
  expect(read).toHaveBeenCalledTimes(1);
  resolve(result);
  await expect(pending).resolves.toEqual(result);
  expect(queryAborted).toBe(true);
  expect(send.mock.calls[0][1].aborted).toBe(true);
  await vi.advanceTimersByTimeAsync(5_000);
  expect(read).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

it("aborts a still-waiting POST when polling confirms completion, and starts no further query", async () => {
  vi.useFakeTimers();
  let postAborted = false;
  const send = vi.fn((_id: string, signal: AbortSignal) => new Promise<ServerImportResult>((_done, reject) => {
    signal.addEventListener("abort", () => { postAborted = true; reject(signal.reason); }, { once: true });
  }));
  const read = vi.fn(async (id: string, _signal: AbortSignal) => progress(id, "completed", { result }));
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  const assertion = expect(pending).resolves.toEqual(result);
  await vi.runAllTimersAsync();
  await assertion;
  expect(postAborted).toBe(true);
  expect(read).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(5_000);
  expect(read).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

it("finishes an immediately successful request without leaving the polling startup timer", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal) => result);
  const read = vi.fn(async (id: string, _signal: AbortSignal) => progress(id, "completed", { result }));
  await expect(trackArchiveRequest("import", send, read, fail, () => false)).resolves.toEqual(result);
  await vi.advanceTimersByTimeAsync(5_000);
  expect(read).not.toHaveBeenCalled();
  expect(vi.getTimerCount()).toBe(0);
});

it("rejects another task's terminal result instead of adopting its outcome", async () => {
  vi.useFakeTimers();
  const send = vi.fn(async (_id: string, _signal: AbortSignal): Promise<ServerImportResult> => { throw new TypeError("synthetic response lost"); });
  const read = vi.fn(async (_id: string, _signal: AbortSignal) => progress("different-synthetic-task", "completed", { result }));
  const pending = trackArchiveRequest("import", send, read, fail, () => false);
  const assertion = expect(pending).rejects.toMatchObject({ code: "TASK_STATUS_UNKNOWN", taskId: send.mock.calls[0][0] });
  await vi.runAllTimersAsync();
  await assertion;
  expect(send).toHaveBeenCalledTimes(1);
  expect(read).toHaveBeenCalledTimes(3);
});
