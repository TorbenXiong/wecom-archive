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

export interface DirectorySelection {
  handle: string;
  displayPath: string;
}

export interface CollectionSummary {
  exportId: string;
  generatedAt: string;
  messageCount: number;
  mediaCount: number;
  missingMediaCount: number;
  contentSha256Prefix: string;
}

export interface ClientExportResult {
  fileName: string;
  format: "json" | "csv";
  messageCount: number;
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
  portableRoot: "…\\userData",
  portableRootWritable: true,
  sourceKeySaved: false,
  automaticRefresh: true,
  runtimeNetworkEnabled: false,
  implementationStage: "browser-preview",
};

export const backend = {
  isNative(): boolean {
    return Boolean(window.__TAURI_INTERNALS__);
  },

  async bootstrap(): Promise<BootstrapState> {
    if (!this.isNative()) return browserBootstrap;
    return invoke<BootstrapState>("bootstrap");
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

  async pickDirectory(purpose: "source" | "export" | "portable_root"): Promise<DirectorySelection | undefined> {
    if (!this.isNative()) return { handle: `preview-${purpose}`, displayPath: "…\\已选择目录" };
    return (await invoke<DirectorySelection | null>("pick_directory", { purpose })) ?? undefined;
  },

  async discoverSelectedSource(selectionHandle: string): Promise<SourceCandidate[]> {
    if (!this.isNative()) return this.discoverSources("…\\已选择目录");
    return invoke<SourceCandidate[]>("discover_selected_source", { selectionHandle });
  },

  async setPortableRoot(selectionHandle: string): Promise<BootstrapState> {
    if (!this.isNative()) return browserBootstrap;
    return invoke<BootstrapState>("set_portable_root", { selectionHandle });
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

  async exportLatest(format: "json" | "csv", selectionHandle: string): Promise<ClientExportResult> {
    if (!this.isNative()) {
      return { fileName: `client-export-preview.${format}`, format, messageCount: 238 };
    }
    return invoke<ClientExportResult>("export_latest", { format, selectionHandle });
  },

  async openExportFolder(): Promise<void> {
    if (!this.isNative()) return;
    await invoke("open_export_folder");
  },

  async clearSavedKey(): Promise<void> {
    if (!this.isNative()) return;
    await invoke("clear_saved_key");
  },
};
