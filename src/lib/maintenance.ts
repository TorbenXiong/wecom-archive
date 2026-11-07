import { invoke } from "@tauri-apps/api/core";

export interface MaintenanceResult {
  directory: string;
  fileCount: number;
  bytes: number;
}

export const openDataMaintenance = () => invoke<void>("open_data_maintenance");
export const maintenanceStatus = () => invoke<boolean>("maintenance_status");
export const createBackup = () => invoke<MaintenanceResult | null>("maintenance_create_backup");
export const verifyBackup = () => invoke<MaintenanceResult | null>("maintenance_verify_backup");
export const restoreBackup = () => invoke<MaintenanceResult | null>("maintenance_restore_backup");
export const activateRestored = () => invoke<void>("maintenance_activate");
