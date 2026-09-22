import { invoke } from "@tauri-apps/api/core";
import type { BootstrapState, SourceCandidate } from "../domain/types";

interface SourceCandidateWire {
  sourceId: string;
  displayPath: string;
  clientVersion?: string;
  capability: SourceCandidate["capability"];
  databases: Array<{
    kind: string;
    encrypted: boolean;
    pageSizeHint?: number;
  }>;
}

export interface CollectionSummary {
  exportId: string;
  generatedAt: string;
  messageCount: number;
  mediaCount: number;
  missingMediaCount: number;
  contentSha256Prefix: string;
}

export interface UploadResult {
  messageCount: number;
}

export interface OfflineExportResult {
  fileName: string;
  directory: string;
  messageCount: number;
}

export interface CollectorScheduleStatus {
  running: boolean;
  lastAttemptAt?: string;
  lastSuccessAt?: string;
  lastError?: string;
  lastMessageCount?: number;
  lastMediaCount?: number;
}

/**
 * Tauri serializes command failures as a plain object, while browser preview
 * and older runtimes may reject with an Error or a JSON string. Keep the
 * conversion in one place so the UI never falls back to an opaque
 * "diagnostic information redacted" message when a safe error code exists.
 */
export interface BackendCommandError {
  code?: string;
  message?: string;
  recoverable?: boolean;
}

export function formatBackendError(reason: unknown, fallback: string): string {
  const parsed = parseBackendError(reason);
  if (!parsed.message && !parsed.code) return fallback;
  if (parsed.code && parsed.message) return `${parsed.message}（错误码：${parsed.code}）`;
  return parsed.message ?? `操作失败（错误码：${parsed.code}）`;
}

function parseBackendError(reason: unknown): BackendCommandError {
  if (reason && typeof reason === "object") {
    const candidate = reason as BackendCommandError;
    if (typeof candidate.code === "string" || typeof candidate.message === "string") {
      return candidate;
    }
  }
  const raw = reason instanceof Error ? reason.message : typeof reason === "string" ? reason : "";
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (parsed && typeof parsed === "object") return parseBackendError(parsed);
  } catch {
    // The runtime may return a human-readable string; preserve it as-is.
  }
  return { message: raw };
}

const browserBootstrap: BootstrapState = {
  organizationName: "示例组织",
  collectionNotice: "仅采集您有权归档的企业微信记录。",
  offlineExportEnabled: true,
  collectorSchedule: { mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" },
};

export const backend = {
  isNative(): boolean {
    return Boolean(window.__TAURI_INTERNALS__);
  },

  async bootstrap(): Promise<BootstrapState> {
    if (!this.isNative()) return browserBootstrap;
    return invoke<BootstrapState>("bootstrap");
  },

  async getCollectorScheduleStatus(): Promise<CollectorScheduleStatus> {
    if (!this.isNative()) return { running: false };
    return invoke<CollectorScheduleStatus>("get_collector_schedule_status");
  },

  async hideCollectorWindow(): Promise<void> {
    if (!this.isNative()) return;
    await invoke("hide_collector_window");
  },

  async discoverSources(selectedRoot?: string): Promise<SourceCandidate[]> {
    if (!this.isNative()) {
      return [
        {
          sourceId: "src-preview",
          displayPath: selectedRoot || "…\\Data",
          clientVersion: "5.0.10.6025",
          capability: "probe_required",
          databases: [
            { kind: "message", encrypted: true, pageSizeHint: 4096 },
            { kind: "session", encrypted: true, pageSizeHint: 4096 },
            { kind: "user", encrypted: true, pageSizeHint: 4096 },
          ],
        },
      ];
    }
    const items = await invoke<SourceCandidateWire[]>("discover_sources");
    return items.map((item) => ({
      sourceId: item.sourceId,
      displayPath: item.displayPath,
      clientVersion: item.clientVersion,
      capability: item.capability,
      databases: item.databases.map((database) => ({
        kind: database.kind,
        encrypted: database.encrypted,
        pageSizeHint: database.pageSizeHint,
      })),
    }));
  },

  async collectSourceAutomatically(sourceId: string): Promise<CollectionSummary> {
    if (!this.isNative()) {
      return {
        exportId: "preview-export",
        generatedAt: new Date().toISOString(),
        messageCount: 238,
        mediaCount: 36,
        missingMediaCount: 0,
        contentSha256Prefix: "8f3ac912d741",
      };
    }
    return invoke<CollectionSummary>("collect_source", {
      sourceId,
    });
  },

  async uploadLatest(): Promise<UploadResult> {
    if (!this.isNative()) return { messageCount: 238 };
    return invoke<UploadResult>("upload_latest_enterprise");
  },

  async exportLatestEncrypted(): Promise<OfflineExportResult> {
    if (!this.isNative()) return { fileName: "WeComArchive-preview.wca", directory: "…\\采集端目录", messageCount: 238 };
    return invoke<OfflineExportResult>("export_latest_enterprise");
  },

  async openOfflineExportDirectory(): Promise<void> {
    if (!this.isNative()) return;
    await invoke("open_offline_export_directory");
  },
};
