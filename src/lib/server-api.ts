import type { MessageDirection, MessageType } from "../domain/types";

export interface ServerArchiveSummary {
  conversation_count: number;
  message_count: number;
  media_count: number;
  revision: number;
  local_collection_available: boolean;
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
  size_bytes?: number;
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
}

export interface GlobalSearchResult extends ServerMessage {
  conversation_name?: string;
  offset_in_conversation: number;
}

export interface ServerParticipant {
  participant_id: string;
  display_name?: string;
  participant_kind?: string;
}

export interface MessageQuery {
  conversationId: string;
  participantId?: string;
  text?: string;
  messageType?: MessageType;
  mediaOnly?: boolean;
  limit?: number;
  offset?: number;
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
  collectionNotice: string;
  uploadUrl: string;
  keyId: string;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
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
  scope: "current_conversation" | "current_filter" | "entire_archive";
  format: "json" | "csv" | "html" | "pdf";
  conversationId?: string;
  participantId?: string;
  text?: string;
  messageType?: MessageType;
  mediaOnly?: boolean;
  dataRedaction?: boolean;
  simplify?: boolean;
  pretty?: boolean;
}

export interface ServerExportResult {
  exportId: string;
  fileName: string;
  directory: string;
  messageCount: number;
  mediaCount: number;
  missingMediaCount: number;
  manifestSha256: string;
}

export interface LocalExportRequest {
  format: "json" | "csv" | "html" | "pdf";
  dataRedaction: boolean;
  simplify?: boolean;
  pretty?: boolean;
}

interface ServerError {
  code?: string;
  message?: string;
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
    throw new Error(detail.message || `服务端请求失败（${response.status}）`);
  }
  return response.json() as Promise<T>;
}

export function getArchiveSummary(token: string): Promise<ServerArchiveSummary> {
  return request<ServerArchiveSummary>("/api/v1/archive/summary", token);
}

export function collectLocalArchive(token: string, includeMedia: boolean): Promise<ServerImportResult> {
  const parameters = new URLSearchParams({ include_media: String(includeMedia) });
  return request<ServerImportResult>(`/api/v1/collections/local?${parameters}`, token, { method: "POST" });
}

export function getLocalCollectionProgress(token: string, signal?: AbortSignal): Promise<LocalCollectionProgress> {
  return request<LocalCollectionProgress>("/api/v1/collections/local/progress", token, { signal });
}

export function listConversations(token: string, signal?: AbortSignal): Promise<ServerConversation[]> {
  return request<ServerConversation[]>("/api/v1/conversations?limit=200", token, { signal });
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

export function searchMessages(token: string, text: string, signal?: AbortSignal): Promise<GlobalSearchResult[]> {
  const parameters = new URLSearchParams({ q: text, limit: "50" });
  return request<GlobalSearchResult[]>(`/api/v1/search/messages?${parameters}`, token, { signal });
}

export async function importClientJson(file: File, token: string): Promise<ServerImportResult> {
  return request<ServerImportResult>("/api/v1/imports/json", token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: file,
  });
}

export function getEnterpriseConfig(token: string): Promise<EnterpriseConfig> {
  return request<EnterpriseConfig>("/api/v1/enterprise/config", token);
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
  collectionNotice: string;
  uploadUrl: string;
  keyId?: string;
  includeMedia?: boolean;
  dataRedaction?: boolean;
  offlineExportEnabled?: boolean;
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

export async function importEnterprisePackage(file: File, token: string): Promise<ServerImportResult> {
  return request<ServerImportResult>("/api/v1/imports/enterprise", token, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream" },
    body: file,
  });
}

export async function getMediaObjectUrl(token: string, contentHash: string): Promise<string> {
  const response = await fetch(`/api/v1/media/${encodeURIComponent(contentHash)}`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!response.ok) throw new Error("媒体内容尚未上传或不可用。");
  return URL.createObjectURL(await response.blob());
}

export function openMediaFile(token: string, contentHash: string, name: string): Promise<{ opened: boolean }> {
  const parameters = new URLSearchParams({ name });
  return request<{ opened: boolean }>(`/api/v1/media/${encodeURIComponent(contentHash)}/open?${parameters}`, token, { method: "POST" });
}

export function createServerExport(token: string, exportRequest: ServerExportRequest): Promise<ServerExportResult> {
  return request<ServerExportResult>("/api/v1/exports", token, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(exportRequest),
  });
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
