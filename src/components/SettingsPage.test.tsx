import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { SettingsPage } from "./SettingsPage";

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("persists the super admin switch immediately", async () => {
  const onChange = vi.fn();
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
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
