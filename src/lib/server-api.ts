import type { ExportScope, MessageDirection, MessageType, ParticipantProfile } from "../domain/types";
import { resumeArchiveTask, trackArchiveRequest, type ArchiveTaskKind, type ArchiveTaskProgress } from "./long-task";

export interface ServerArchiveSummary {
  conversation_count: number;
  message_count: number;
  media_count: number;
  revision: number;
  local_collection_available: boolean;
}

export interface ArchiveUpdate {
  revision: number;
}

export interface ServerHealth {
  status: string;
  schema: string;
  networkMode: "loopback_only" | "configured_remote" | string;
  listenAddress: string;
  version: string;
}

export interface ServerConversation {
  conversation_id: string;
  display_name?: string;
  conversation_type?: string;
  participant_names?: string[];
  last_message_at: string;
  message_count: number;
  media_count: number;
  participant_count: number;
}

export interface ServerMediaRef {
  content_hash?: string;
  original_name?: string;
  mime_type?: string;
  size_bytes?: number | null;
  source_locator: string;
  integrity: "verified" | "missing" | "size_mismatch" | "hash_mismatch" | "unreadable";
  missing_reason?: string;
}

export interface ServerMessage {
  stable_message_id: string;
  conversation_id: string;
  sender_id?: string;
  sender_name?: string;
  sender_kind?: string;
  sent_at: string;
  direction: MessageDirection;
  message_type: MessageType;
  body_text?: string;
  quoted_message_id?: string;
  quoted_message?: {
    stable_message_id: string;
    sender_id?: string;
    sender_name?: string;
    sent_at: string;
    body_text?: string;
    message_type: MessageType;
  };
  lifecycle: "active" | "recalled" | "deleted_at_source" | "unknown";
  media: ServerMediaRef[];
  raw_type: string;
  raw_payload?: {
    rich_segments?: Array<{ kind: "text" | "emoji"; text: string }>;
    [key: string]: unknown;
  };
}

export interface GlobalSearchResult extends ServerMessage {
  conversation_name?: string;
  offset_in_conversation?: number;
}

export interface GlobalSearchFilters {
  conversationId?: string;
  participantId?: string;
  messageType?: MessageType;
  mediaOnly?: boolean;
  startsAt?: string;
  endsAt?: string;
}

export interface ServerParticipant {
  participant_id: string;
  display_name?: string;
  participant_kind?: string;
  profile?: ParticipantProfile;
}

export interface CollectedUser {
  source_instance_id: string;
  display_name?: string;
}

export interface MessageQuery {
  conversationId: string;
  participantId?: string;
  text?: string;
  messageType?: MessageType;
  mediaOnly?: boolean;
  limit?: number;
  offset?: number;
  sort?: "asc" | "desc";
  anchorMessageId?: string;
  anchorShift?: number;
  anchorRevision?: number;
}

export interface ServerImportResult {
  exportId: string;
  batchCount: number;
  inserted: number;
  unchanged: number;
  revised: number;
  mediaCount: number;
  missingMediaCount: number;
}

export interface LocalCollectionProgress {
  running: boolean;
  percent: number;
  stage: string;
  detail: string;
}

export interface EnterpriseConfig {
  configured: boolean;
  organizationId: string;
  organizationName: string;
  uploadUrl: string;
  keyId: string;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
  hiddenModeEnabled: boolean;
  serverLogLevel: LogLevel;
  collectorLogLevel: LogLevel;
  superAdminEnabled: boolean;
  serverSchedule: CollectionSchedule;
  collectorSchedule: CollectionSchedule;
}

export type CollectionScheduleMode = "disabled" | "interval" | "daily";
export type LogLevel = "off" | "normal" | "verbose";

export interface CollectionSchedule {
  mode: CollectionScheduleMode;
  intervalMinutes: number;
  dailyTime: string;
}

export interface CollectionSchedules {
  localPlans: LocalCollectionPlan[];
  collectorPlans: CollectorPlan[];
}

export type CollectionTargetKind = "server" | "collector";
export type CollectionTargetStatus = "online" | "offline" | "disabled" | "revoked";

export interface CollectionPlan {
  id: string;
  name: string;
  includeMedia: boolean;
  schedule: CollectionSchedule;
  createdAt: string;
  updatedAt: string;
  lastRunAt?: string;
  lastUploadAt?: string;
  lastStatus?: "success" | "error";
  lastDetail?: string;
  nextRunAt?: string;
}

export interface CollectionTarget {
  updateCapable?: boolean;
  updateAvailable?: boolean;
  updateVersion?: string;
  updatePending?: boolean;
  updateError?: string;
  targetId: string;
  kind: CollectionTargetKind;
  displayName: string;
  status: CollectionTargetStatus;
  lastSeenAt?: string;
  clientIp?: string;
  clientVersion?: string;
  configRevision?: number;
  lastAppliedRevision?: number;
  lastError?: string;
  includeMedia?: boolean;
  dataRedaction?: boolean;
  offlineExportEnabled?: boolean;
  hiddenModeEnabled?: boolean;
  collectorLogLevel?: LogLevel;
  logDirectory?: string;
  plans: CollectionPlan[];
}

export interface CollectionTargetsResponse {
  targets: CollectionTarget[];
}

export function requestCollectorUpdate(token: string, collectorId: string): Promise<{ status: string; version: string }> {
  return request(`/api/v1/collectors/${encodeURIComponent(collectorId)}/software-update`, token, { method: "POST" });
}

export interface CollectorDesiredConfig {
  collectorId: string;
  displayName: string;
  enabled: boolean;
  configRevision: number;
  organizationId: string;
  organizationName: string;
  uploadUrl: string;
  keyId: string;
  publicKeyHex: string;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
  hiddenModeEnabled: boolean;
  collectorLogLevel: LogLevel;
  schedule: CollectionSchedule;
  manualCollectionRequestId?: string;
}

export interface CollectorControlResponse {
  schemaVersion: "collector-control.v1";
  configRevision: number;
  expiresAt: string;
  config: CollectorDesiredConfig;
  signatureHex: string;
}

export interface LocalCollectionPlan {
  id: string;
  name: string;
  includeMedia: boolean;
  schedule: CollectionSchedule;
  createdAt: string;
  updatedAt: string;
  lastRunAt?: string;
  lastStatus?: "success" | "error";
  lastDetail?: string;
  nextRunAt?: string;
}

export interface CollectorPlan {
  collectorId: string;
  fileName: string;
  organizationName: string;
  uploadUrl: string;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
  hiddenModeEnabled: boolean;
  collectorLogLevel: LogLevel;
  schedule: CollectionSchedule;
  createdAt: string;
  lastUploadAt?: string;
  lastRunAt?: string;
  nextRunAt?: string;
  executableAvailable: boolean;
}

export interface CollectorResult {
  fileName: string;
  directory: string;
  organizationId: string;
  keyId: string;
  artifact: string;
  executableGenerated: boolean;
}

export interface ServerExportRequest {
  scope: ExportScope;
  format: "json" | "csv" | "html" | "md";
  conversationId?: string;
  conversationIds?: string[];
  startDate?: string;
  endDate?: string;
  participantId?: string;
  text?: string;
  messageType?: MessageType;
  mediaOnly?: boolean;
  dataRedaction?: boolean;
  simplify?: boolean;
  pretty?: boolean;
  conversationOrder?: ExportOrder;
  messageOrder?: ExportOrder;
}

export type ExportOrder = "ascending" | "descending";

export interface ServerExportResult {
  taskId?: string;
  status?: string;
  exportId: string;
  fileName: string;
  directory: string;
  messageCount: number;
  mediaCount: number;
  missingMediaCount: number;
  manifestSha256: string;
}

export interface ExportTaskProgress {
  status: "queued" | "running" | "cancelling" | "completed" | "cancelled" | "failed" | string;
  percent: number;
  stage: string;
  result?: ServerExportResult;
  error?: string;
  errorCode?: string;
  processedMessages?: number;
  totalMessages?: number;
}

export interface LocalExportRequest {
  format: "json" | "csv" | "html" | "md";
  sourceRoot?: string;
  dataRedaction: boolean;
  simplify?: boolean;
  pretty?: boolean;
  conversationOrder?: ExportOrder;
  messageOrder?: ExportOrder;
}

interface ServerError {
  code?: string;
  message?: string;
}

export class ServerApiError extends Error {
  constructor(message: string, readonly status: number, readonly code?: string) {
    super(message);
    this.name = "ServerApiError";
  }
}

async function request<T>(path: string, token: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: {
      ...init?.headers,
      Authorization: `Bearer ${token}`,
    },
  });
  if (!response.ok) {
    const detail = await response.json().catch(() => ({})) as ServerError;
    throw new ServerApiError(detail.message || `服务端请求失败（${response.status}）`, response.status, detail.code);
  }
  return response.json() as Promise<T>;
}

async function boundedRequest<T>(path: string, token: string, signal?: AbortSignal): Promise<T> {
  const controller = new AbortController();
  const abort = () => controller.abort(signal?.reason);
  if (signal?.aborted) abort();
  else signal?.addEventListener("abort", abort, { once: true });
  const timer = setTimeout(() => controller.abort(new DOMException("状态查询超时", "TimeoutError")), 30_000);
  try { return await request<T>(path, token, { signal: controller.signal }); }
  finally { clearTimeout(timer); signal?.removeEventListener("abort", abort); }
}

export function getArchiveTaskStatus(token: string, taskId: string, signal?: AbortSignal): Promise<ArchiveTaskProgress> {
  return boundedRequest(`/api/v1/tasks/${encodeURIComponent(taskId)}`, token, signal);
}

function archiveTaskFailure(progress: ArchiveTaskProgress): ServerApiError {
  return new ServerApiError(progress.error || "服务端任务未完成。", progress.errorStatus || 500, progress.errorCode || undefined);
}

function trackedRequest(token: string, kind: ArchiveTaskKind, path: string, init: RequestInit, onProgress?: (progress: ArchiveTaskProgress) => void): Promise<ServerImportResult> {
  return trackArchiveRequest(kind,
    (id, signal) => request<ServerImportResult>(path, token, { ...init, signal, headers: { ...init.headers, "X-Archive-Task-Id": id } }),
    (id, signal) => getArchiveTaskStatus(token, id, signal), archiveTaskFailure,
    (error) => error instanceof ServerApiError && Boolean(error.code) && error.code !== "TASK_ALREADY_EXISTS", onProgress);
}

export function resumeArchiveTaskStatus(token: string, taskId: string, kind: ArchiveTaskKind, onProgress?: (progress: ArchiveTaskProgress) => void): Promise<ServerImportResult> {
  return resumeArchiveTask(taskId, kind, (signal) => getArchiveTaskStatus(token, taskId, signal), archiveTaskFailure, onProgress);
}

export function getArchiveSummary(token: string): Promise<ServerArchiveSummary> {
  return request<ServerArchiveSummary>("/api/v1/archive/summary", token);
}

export function getServerHealth(signal?: AbortSignal): Promise<ServerHealth> {
  return fetch("/api/v1/health", { signal }).then(async (response) => {
    if (!response.ok) throw new Error("无法读取服务端状态。");
    return await response.json() as ServerHealth;
  });
}

export function waitForArchiveUpdate(token: string, since: number, signal: AbortSignal): Promise<ArchiveUpdate> {
  return request<ArchiveUpdate>(
    `/api/v1/archive/updates?since=${encodeURIComponent(since)}`,
    token,
    { signal },
  );
}

export function getCollectedUsers(token: string, signal?: AbortSignal): Promise<CollectedUser[]> {
  return request<CollectedUser[]>("/api/v1/archive/collected-users", token, { signal });
}

export function collectLocalArchive(token: string, includeMedia: boolean, sourceRoot?: string, dataRedaction?: boolean, onProgress?: (progress: ArchiveTaskProgress) => void): Promise<ServerImportResult> {
  const parameters = new URLSearchParams({ include_media: String(includeMedia) });
  if (sourceRoot) parameters.set("source_root", sourceRoot);
  if (dataRedaction !== undefined) parameters.set("data_redaction", String(dataRedaction));
  return trackedRequest(token, "collection", `/api/v1/collections/local?${parameters}`, { method: "POST" }, onProgress);
}

export function getLocalCollectionProgress(token: string, signal?: AbortSignal): Promise<LocalCollectionProgress> {
  return boundedRequest<LocalCollectionProgress>("/api/v1/collections/local/progress", token, signal);
}

export async function listConversations(token: string, signal?: AbortSignal): Promise<ServerConversation[]> {
  const pageSize = 200;
  const conversations: ServerConversation[] = [];
  const seen = new Set<string>();
  for (let offset = 0; ; offset += pageSize) {
    const page = await listConversationPage(token, offset, signal, pageSize);
    let added = 0;
    for (const conversation of page) {
      if (seen.has(conversation.conversation_id)) continue;
      seen.add(conversation.conversation_id);
      conversations.push(conversation);
      added += 1;
    }
    if (page.length < pageSize || added === 0) break;
  }
  return conversations;
}

export const CONVERSATION_PAGE_SIZE = 50;

export function listConversationPage(token: string, offset = 0, signal?: AbortSignal, limit = CONVERSATION_PAGE_SIZE): Promise<ServerConversation[]> {
  return request<ServerConversation[]>(`/api/v1/conversations?limit=${limit}&offset=${offset}`, token, { signal });
}

export function listConversationParticipants(token: string, conversationId: string, signal?: AbortSignal): Promise<ServerParticipant[]> {
  return request<ServerParticipant[]>(`/api/v1/conversations/${encodeURIComponent(conversationId)}/participants`, token, { signal });
}

export function listMessages(token: string, query: MessageQuery, signal?: AbortSignal): Promise<ServerMessage[]> {
  const parameters = new URLSearchParams({
    conversation_id: query.conversationId,
    limit: String(query.limit ?? 200),
    offset: String(query.offset ?? 0),
  });
  if (query.participantId) parameters.set("participant_id", query.participantId);
  if (query.text) parameters.set("text", query.text);
  if (query.messageType) parameters.set("message_type", query.messageType);
  if (query.mediaOnly) parameters.set("media_only", "true");
  if (query.sort) parameters.set("sort", query.sort);
  if (query.anchorMessageId) parameters.set("anchor_message_id", query.anchorMessageId);
  if (query.anchorShift !== undefined) parameters.set("anchor_shift", String(query.anchorShift));
  if (query.anchorRevision !== undefined) parameters.set("anchor_revision", String(query.anchorRevision));
  return request<ServerMessage[]>(`/api/v1/messages?${parameters}`, token, { signal });
}

export function countMessages(token: string, query: Omit<MessageQuery, "limit" | "offset">, signal?: AbortSignal): Promise<{ total: number }> {
  const parameters = new URLSearchParams({ conversation_id: query.conversationId });
  if (query.participantId) parameters.set("participant_id", query.participantId);
  if (query.text) parameters.set("text", query.text);
  if (query.messageType) parameters.set("message_type", query.messageType);
  if (query.mediaOnly) parameters.set("media_only", "true");
  return request<{ total: number }>(`/api/v1/messages/count?${parameters}`, token, { signal });
}

export function searchMessages(token: string, text: string, signal?: AbortSignal, sort: "asc" | "desc" = "desc", offset = 0, filters: GlobalSearchFilters = {}): Promise<GlobalSearchResult[]> {
  const parameters = new URLSearchParams({ q: text, limit: "50", offset: String(offset), sort, locate: "false" });
  if (filters.conversationId) parameters.set("conversation_id", filters.conversationId);
  if (filters.participantId) parameters.set("participant_id", filters.participantId);
  if (filters.messageType) parameters.set("message_type", filters.messageType);
  if (filters.mediaOnly) parameters.set("media_only", "true");
  if (filters.startsAt) parameters.set("starts_at", filters.startsAt);
  if (filters.endsAt) parameters.set("ends_at", filters.endsAt);
  return request<GlobalSearchResult[]>(`/api/v1/search/messages?${parameters}`, token, { signal });
}

export function getMessagePosition(token: string, stableMessageId: string, sort: "asc" | "desc" = "desc", signal?: AbortSignal): Promise<{ conversation_id: string; offset_in_conversation: number }> {
  return request<{ conversation_id: string; offset_in_conversation: number }>(`/api/v1/messages/${encodeURIComponent(stableMessageId)}/position?sort=${sort}`, token, { signal });
}

export async function importClientJson(file: File, token: string, onProgress?: (progress: ArchiveTaskProgress) => void): Promise<ServerImportResult> {
  return trackedRequest(token, "import", "/api/v1/imports/json", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: file,
  }, onProgress);
}

export function getEnterpriseConfig(token: string): Promise<EnterpriseConfig> {
  return request<EnterpriseConfig>("/api/v1/enterprise/config", token);
}

export function updateSuperAdmin(token: string, enabled: boolean): Promise<{ superAdminEnabled: boolean }> {
  return request<{ superAdminEnabled: boolean }>("/api/v1/settings/super-admin", token, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ enabled }),
  });
}

export function getCollectionTargets(token: string): Promise<CollectionTargetsResponse> {
  return request<CollectionTargetsResponse>("/api/v1/collection-targets", token);
}

export function getCollector(token: string, collectorId: string): Promise<{ collector: CollectorPlan; status: CollectionTargetStatus; config: CollectorDesiredConfig }> {
  return request<{ collector: CollectorPlan; status: CollectionTargetStatus; config: CollectorDesiredConfig }>(`/api/v1/collectors/${encodeURIComponent(collectorId)}`, token);
}

export function updateCollector(token: string, collectorId: string, input: {
  displayName: string;
  enabled: boolean;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
  hiddenModeEnabled?: boolean;
  collectorLogLevel?: LogLevel;
  schedule: CollectionSchedule;
}): Promise<{ config: CollectorDesiredConfig; status: string }> {
  return request<{ config: CollectorDesiredConfig; status: string }>(`/api/v1/collectors/${encodeURIComponent(collectorId)}`, token, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
  });
}

export function revokeCollector(token: string, collectorId: string): Promise<{ revoked: boolean }> {
  return request<{ revoked: boolean }>(`/api/v1/collectors/${encodeURIComponent(collectorId)}/revoke`, token, { method: "POST" });
}

export function requestCollectorCollection(token: string, collectorId: string): Promise<{ queued: boolean; requestId: string }> {
  return request<{ queued: boolean; requestId: string }>(`/api/v1/collectors/${encodeURIComponent(collectorId)}/collect`, token, { method: "POST" });
}

export function createLocalCollectionPlan(token: string, input: { name: string; includeMedia: boolean; schedule: CollectionSchedule }): Promise<CollectionSchedules> {
  return request<CollectionSchedules>("/api/v1/collection-schedules", token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
  });
}

export function updateLocalCollectionPlan(token: string, planId: string, input: { name: string; includeMedia: boolean; schedule: CollectionSchedule }): Promise<CollectionSchedules> {
  return request<CollectionSchedules>(`/api/v1/collection-schedules/${encodeURIComponent(planId)}`, token, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
  });
}

export function deleteLocalCollectionPlan(token: string, planId: string): Promise<CollectionSchedules> {
  return request<CollectionSchedules>(`/api/v1/collection-schedules/${encodeURIComponent(planId)}`, token, { method: "DELETE" });
}

export function listEnterpriseCollectors(token: string): Promise<{ collectors: CollectorPlan[] }> {
  return request<{ collectors: CollectorPlan[] }>("/api/v1/enterprise/collectors", token);
}

export function updateServerAccessToken(currentToken: string, accessToken: string): Promise<{ updated: boolean }> {
  return request<{ updated: boolean }>("/api/v1/server/access-token", currentToken, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ accessToken }),
  });
}

export function regenerateServerAccessToken(currentToken: string): Promise<{ accessToken: string }> {
  return request<{ accessToken: string }>("/api/v1/server/access-token/regenerate", currentToken, {
    method: "POST",
  });
}

export function updateEnterpriseConfig(token: string, config: {
  organizationName: string;
  uploadUrl: string;
  keyId?: string;
  includeMedia?: boolean;
  dataRedaction?: boolean;
  offlineExportEnabled?: boolean;
  hiddenModeEnabled?: boolean;
  serverLogLevel?: LogLevel;
  collectorLogLevel?: LogLevel;
  superAdminEnabled?: boolean;
  serverSchedule?: CollectionSchedule;
  collectorSchedule?: CollectionSchedule;
}): Promise<EnterpriseConfig> {
  return request<EnterpriseConfig>("/api/v1/enterprise/config", token, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(config),
  });
}

export function rotateEnterpriseKey(token: string): Promise<EnterpriseConfig> {
  return request<EnterpriseConfig>("/api/v1/enterprise/key/rotate", token, { method: "POST" });
}

export function generateEnterpriseCollector(token: string): Promise<CollectorResult> {
  return request<CollectorResult>("/api/v1/enterprise/collectors", token, { method: "POST" });
}

export function openEnterpriseCollectorDirectory(token: string): Promise<{ opened: boolean }> {
  return request<{ opened: boolean }>("/api/v1/enterprise/collectors/open-directory", token, { method: "POST" });
}

export async function importEnterprisePackage(file: File, token: string, onProgress?: (progress: ArchiveTaskProgress) => void): Promise<ServerImportResult> {
  return trackedRequest(token, "import", "/api/v1/imports/enterprise", {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream" },
    body: file,
  }, onProgress);
}

export async function getMediaBlob(token: string, contentHash: string, signal?: AbortSignal): Promise<Blob> {
  const response = await fetch(`/api/v1/media/${encodeURIComponent(contentHash)}`, {
    headers: { Authorization: `Bearer ${token}` },
    signal,
  });
  if (!response.ok) throw new Error("媒体内容尚未上传或不可用。");
  return response.blob();
}

export async function getMediaObjectUrl(token: string, contentHash: string, signal?: AbortSignal): Promise<string> {
  const blob = await getMediaBlob(token, contentHash, signal);
  signal?.throwIfAborted();
  return URL.createObjectURL(blob);
}

export function openMediaFile(token: string, contentHash: string, name: string): Promise<{ opened: boolean }> {
  const parameters = new URLSearchParams({ name });
  return request<{ opened: boolean }>(`/api/v1/media/${encodeURIComponent(contentHash)}/open?${parameters}`, token, { method: "POST" });
}

export class ExportStatusUnknownError extends ServerApiError {
  constructor(readonly taskId: string, readonly cancellationRequested: boolean, message?: string) {
    super(message || "暂时无法确认导出结果，原任务可能仍在运行。请继续查询原任务。", 0, "EXPORT_STATUS_UNKNOWN");
    this.name = "ExportStatusUnknownError";
  }
}

function retryableExportStatusError(error: unknown): boolean {
  return !(error instanceof ServerApiError) || error.status >= 500 || error.status === 408 || error.status === 429;
}

async function queryExportWithRetry(taskId: string, cancellationRequested: () => boolean, action: () => Promise<ExportTaskProgress>): Promise<ExportTaskProgress> {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      const progress = await action();
      if (!["queued", "running", "cancelling", "completed", "cancelled", "failed"].includes(progress.status)
        || (progress.status === "completed" && !progress.result)) throw new Error("Invalid export status");
      return progress;
    } catch (error) {
      if (!retryableExportStatusError(error) || attempt === 2) {
        throw new ExportStatusUnknownError(taskId, cancellationRequested(),
          error instanceof ServerApiError && (error.status === 401 || error.status === 403)
            ? "无法读取导出状态，请核对访问权限后继续查询原任务。" : undefined);
      }
      await new Promise<void>((resolve) => window.setTimeout(resolve, 250 * (attempt + 1)));
    }
  }
  throw new ExportStatusUnknownError(taskId, cancellationRequested());
}

function finishedExport(progress: ExportTaskProgress): ServerExportResult | undefined {
  if (progress.status === "completed" && progress.result) return progress.result;
  if (progress.status === "cancelled") throw new ServerApiError("导出已取消，未完成文件已清理。", 409, "EXPORT_CANCELLED");
  if (progress.status === "failed") throw new ServerApiError(progress.error || "服务端导出失败。", 500, progress.errorCode);
  return undefined;
}

export async function createServerExport(token: string, exportRequest: ServerExportRequest, signal?: AbortSignal, onProgress?: (progress: ExportTaskProgress) => void, resumeTaskId?: string): Promise<ServerExportResult> {
  if (!resumeTaskId) signal?.throwIfAborted();
  let taskId = resumeTaskId;
  if (!taskId) {
    const requestedTaskId = crypto.randomUUID();
    try {
      const started = await request<{ taskId: string }>("/api/v1/exports/start", token, {
        method: "POST", headers: { "Content-Type": "application/json", "X-Archive-Task-Id": requestedTaskId }, body: JSON.stringify(exportRequest), signal: AbortSignal.timeout(30000),
      });
      if (typeof started.taskId !== "string" || !started.taskId.trim()) {
        throw new ExportStatusUnknownError(requestedTaskId, Boolean(signal?.aborted));
      }
      // Older servers may ignore the optional identity header and return their
      // own ID. A valid acknowledgement remains the authority in that case.
      taskId = started.taskId;
    } catch (error) {
      if (error instanceof ServerApiError && error.status === 404) return request<ServerExportResult>("/api/v1/exports", token, {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(exportRequest), signal,
      });
      // Capacity is a confirmed refusal before registration. Other coded 5xx
      // replies may come from a gateway after the original task was accepted.
      if (error instanceof ServerApiError && error.code && error.code !== "TASK_ALREADY_EXISTS"
        && ((error.status >= 400 && error.status < 500 && ![408, 409, 429].includes(error.status))
          || (error.status === 503 && error.code === "EXPORT_TASK_CAPACITY_REACHED"))) throw error;
      throw new ExportStatusUnknownError(requestedTaskId, Boolean(signal?.aborted));
    }
  }
  let cancelSent = false;
  while (true) {
    if (signal?.aborted && !cancelSent) {
      // Cancellation is idempotent; a lost response can safely be retried.
      const cancelling = await queryExportWithRetry(taskId, () => true, () => cancelServerExport(token, taskId));
      onProgress?.(cancelling);
      cancelSent = true;
      const completed = finishedExport(cancelling);
      if (completed) return completed;
    }
    const progress = await queryExportWithRetry(taskId, () => Boolean(signal?.aborted), () =>
      request<ExportTaskProgress>(`/api/v1/exports/${encodeURIComponent(taskId)}`, token, { signal: AbortSignal.timeout(30000) }));
    onProgress?.(progress);
    const completed = finishedExport(progress);
    if (completed) return completed;
    await new Promise<void>((resolve) => window.setTimeout(resolve, 250));
  }
}

export function cancelServerExport(token: string, taskId: string): Promise<ExportTaskProgress> {
  return request<ExportTaskProgress>(`/api/v1/exports/${encodeURIComponent(taskId)}/cancel`, token, { method: "POST", signal: AbortSignal.timeout(30000) });
}

export function createLocalExport(token: string, payload: LocalExportRequest): Promise<ServerExportResult> {
  return request<ServerExportResult>("/api/v1/exports/local", token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

export function openExportDirectory(token: string): Promise<{ opened: boolean }> {
  return request<{ opened: boolean }>("/api/v1/exports/open-directory", token, { method: "POST" });
}
