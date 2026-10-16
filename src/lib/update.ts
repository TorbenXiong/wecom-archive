import { invoke } from "@tauri-apps/api/core";

export interface UpdateInfo {
  currentVersion: string;
  latestVersion: string;
  releaseUrl: string;
  assetName: string;
  sha256: string;
}

export function checkForUpdate(): Promise<UpdateInfo | null> {
  return invoke<UpdateInfo | null>("check_for_update");
}

export function installUpdate(update: UpdateInfo): Promise<void> {
  return invoke("install_update", { version: update.latestVersion });
}
