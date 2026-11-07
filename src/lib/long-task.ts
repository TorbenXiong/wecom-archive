import type { ServerImportResult } from "./server-api";

export type ArchiveTaskKind = "import" | "collection";
export interface ArchiveTaskProgress {
  taskId: string;
  kind: ArchiveTaskKind;
  status: "queued" | "receiving" | "running" | "completed" | "failed";
  stage: string;
  receivedBytes: number;
  startedAt: string;
  updatedAt: string;
  result?: ServerImportResult | null;
  error?: string | null;
  errorCode?: string | null;
  errorStatus?: number | null;
  trackingUnavailable?: boolean;
}

export class ArchiveTaskPendingError extends Error {
  readonly code = "TASK_STATUS_UNKNOWN";
  constructor(readonly taskId: string, readonly kind: ArchiveTaskKind) {
    super("暂时无法确认任务结果，任务可能仍在运行。请继续查询原任务，或核对归档后再决定是否重新执行。");
    this.name = "ArchiveTaskPendingError";
  }
}

type Reader = (signal: AbortSignal) => Promise<ArchiveTaskProgress>;
type Progress = (progress: ArchiveTaskProgress) => void;
type Failure = (progress: ArchiveTaskProgress) => Error;

function pause(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    signal.throwIfAborted();
    const abort = () => { clearTimeout(timer); reject(signal.reason); };
    const timer = setTimeout(() => { signal.removeEventListener("abort", abort); resolve(); }, ms);
    signal.addEventListener("abort", abort, { once: true });
  });
}

async function poll(
  id: string, kind: ArchiveTaskKind, read: Reader, fail: Failure, onProgress: Progress | undefined,
  signal: AbortSignal, responseLost: () => boolean,
): Promise<ServerImportResult> {
  let errors = 0;
  let last: ArchiveTaskProgress | undefined;
  while (true) {
    let progress: ArchiveTaskProgress;
    try {
      progress = await read(signal);
      if (progress.taskId !== id || progress.kind !== kind || !["queued", "receiving", "running", "completed", "failed"].includes(progress.status)) {
        throw new Error("Invalid task status");
      }
      if (progress.status === "completed" && !progress.result) throw new Error("Missing task result");
      errors = 0;
    } catch {
      signal.throwIfAborted();
      errors += 1;
      if (last) onProgress?.({ ...last, trackingUnavailable: true });
      // A legacy server may lack status queries while its original request is
      // healthy. Keep awaiting that request; never submit a second operation.
      if (responseLost() && errors >= 3) throw new ArchiveTaskPendingError(id, kind);
      await pause(Math.min(errors * 750, 3000), signal);
      continue;
    }
    last = progress;
    onProgress?.({ ...progress, trackingUnavailable: false });
    if (progress.status === "completed") return progress.result!;
    if (progress.status === "failed") throw fail(progress);
    await pause(750, signal);
  }
}

export async function trackArchiveRequest(
  kind: ArchiveTaskKind,
  send: (id: string, signal: AbortSignal) => Promise<ServerImportResult>,
  read: (id: string, signal: AbortSignal) => Promise<ArchiveTaskProgress>,
  fail: Failure,
  definitive: (error: unknown) => boolean,
  onProgress?: Progress,
): Promise<ServerImportResult> {
  const id = crypto.randomUUID();
  const controller = new AbortController();
  const now = new Date().toISOString();
  onProgress?.({ taskId: id, kind, status: "queued", stage: "等待服务端确认", receivedBytes: 0, startedAt: now, updatedAt: now });
  let lost = false;
  const response = send(id, controller.signal).catch((error: unknown) => {
    if (definitive(error)) throw error;
    lost = true;
    // The polling branch owns the outcome when the response is lost. A lost
    // POST response is not evidence of rollback and must not trigger a retry.
    return new Promise<never>(() => {});
  });
  const status = (async () => {
    await pause(500, controller.signal);
    return poll(id, kind, (signal) => read(id, signal), fail, onProgress, controller.signal, () => lost);
  })();
  try { return await Promise.race([response, status]); }
  finally { controller.abort(); }
}

export async function resumeArchiveTask(
  id: string, kind: ArchiveTaskKind, read: Reader, fail: Failure, onProgress?: Progress,
): Promise<ServerImportResult> {
  const controller = new AbortController();
  try { return await poll(id, kind, read, fail, onProgress, controller.signal, () => true); }
  finally { controller.abort(); }
}
