import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { SettingsPage } from "./SettingsPage";
import { openDataMaintenance } from "../lib/maintenance";

vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true, invoke: vi.fn() }));
vi.mock("../lib/maintenance", () => ({ openDataMaintenance: vi.fn() }));

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("opens maintenance only on request and reports a launch failure", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ status: "ok", networkMode: "loopback_only" })));
  vi.mocked(openDataMaintenance).mockRejectedValue("synthetic launch failure");
  render(<SettingsPage token="token" superAdminEnabled onSuperAdminChange={vi.fn()} />);
  expect(openDataMaintenance).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "退出工作台并打开备份与恢复" }));
  await screen.findByText("无法打开备份与恢复窗口，请重试。");
  expect(screen.getByRole("button", { name: "退出工作台并打开备份与恢复" })).toBeEnabled();
});

it("persists the super admin switch immediately", async () => {
  const onChange = vi.fn();
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    if (!init?.method) return Response.json({ status: "ok", schema: "server-api.v1", networkMode: "loopback_only", listenAddress: "127.0.0.1:9812", version: "0.0.9" });
    expect(init?.method).toBe("PUT");
    expect(JSON.parse(String(init?.body))).toEqual({ enabled: true });
    return Response.json({ superAdminEnabled: true });
  });
  vi.stubGlobal("fetch", fetchMock);
  render(<SettingsPage token="token" superAdminEnabled={false} onSuperAdminChange={onChange} />);

  fireEvent.click(screen.getByRole("checkbox", { name: "启用超管模式" }));
  await waitFor(() => expect(onChange).toHaveBeenCalledWith(true));
  expect(fetchMock).toHaveBeenCalledWith("/api/v1/settings/super-admin", expect.objectContaining({ method: "PUT" }));
});
