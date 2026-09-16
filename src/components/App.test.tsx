import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, vi } from "vitest";
import App from "../App";

const accessToken = "0123456789abcdefghijklmnop";

function installApiMock() {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/archive/summary")) {
      return Response.json({ conversation_count: 1, message_count: 1, media_count: 0, revision: 1, local_collection_available: true });
    }
    if (url.includes("/conversations")) {
      return Response.json([{
        conversation_id: "conversation-1",
        display_name: "真实 API 会话",
        conversation_type: "group",
        last_message_at: "2026-09-07T10:24:00Z",
        message_count: 1,
        media_count: 0,
        participant_count: 3,
      }]);
    }
    if (url.includes("/messages/count")) {
      return Response.json({ total: 1 });
    }
    if (url.includes("/messages")) {
      return Response.json([{
        stable_message_id: "message-1",
        conversation_id: "conversation-1",
        sender_id: "participant-1",
        sent_at: "2026-09-07T10:24:00Z",
        direction: "incoming",
        message_type: "text",
        body_text: "来自服务端归档的数据",
        lifecycle: "active",
        media: [],
        raw_type: "text",
      }]);
    }
    if (url.includes("/enterprise/config")) {
      const body = init?.body ? JSON.parse(String(init.body)) as { keyId?: string } : undefined;
      return Response.json({ configured: false, organizationId: "org-1", organizationName: "", collectionNotice: "加密传输本机企业微信聊天记录到服务端", uploadUrl: "http://127.0.0.1:8787", keyId: body?.keyId || "key-1", includeMedia: false, dataRedaction: false, offlineExportEnabled: false });
    }
    if (url.includes("/enterprise/collectors")) {
      return Response.json({ fileName: "key-1.exe", directory: "D:\\serverData\\collectors", organizationId: "org-1", keyId: "key-1", collectorId: "collector-1", artifact: "{}", executableGenerated: true });
    }
    if (url.includes("/imports/enterprise")) {
      return Response.json({ exportId: "export-1", batchCount: 1, inserted: 2, unchanged: 0, revised: 0 });
    }
    if (url.includes("/imports/json")) {
      return Response.json({ exportId: "json-export-1", batchCount: 1, inserted: 2, unchanged: 0, revised: 0 });
    }
    if (url.includes("/collections/local")) {
      return Response.json({ exportId: "local-export-1", batchCount: 1, inserted: 3, unchanged: 1, revised: 1 });
    }
    if (url.includes("/exports/local")) {
      return Response.json({ exportId: "local-file-1", fileName: "local-export.csv", directory: "D:\\serverData\\exports", messageCount: 238, mediaCount: 0, missingMediaCount: 0, manifestSha256: "hash" });
    }
    if (url.includes("/exports/open-directory")) return Response.json({ opened: true });
    return Response.json({}, { status: 404 });
  }));
}

async function connect() {
  fireEvent.change(screen.getByLabelText("服务端访问令牌"), { target: { value: accessToken } });
  fireEvent.click(screen.getByRole("button", { name: "验证并进入工作台" }));
  await screen.findByRole("heading", { name: "真实 API 会话", level: 1 });
}

afterEach(() => {
  window.history.replaceState(null, "", "/");
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("archive workspace", () => {
  it("shows startup progress instead of the access form while automatic authentication is pending", async () => {
    installApiMock();
    const apiFetch = vi.mocked(fetch).getMockImplementation()!;
    let releaseRequests!: () => void;
    const pending = new Promise<void>((resolve) => { releaseRequests = resolve; });
    vi.mocked(fetch).mockImplementation(async (...args) => {
      await pending;
      return apiFetch(...args);
    });
    window.history.replaceState(null, "", `/?view=archive#token=${accessToken}`);
    const storageSpy = vi.spyOn(Storage.prototype, "setItem");

    render(<StrictMode><App /></StrictMode>);
    expect(screen.getByRole("status")).toHaveTextContent("正在启动归档工作台");
    expect(screen.queryByLabelText("服务端访问令牌")).not.toBeInTheDocument();
    expect(screen.queryByText("连接归档工作台")).not.toBeInTheDocument();

    releaseRequests();
    expect(await screen.findByRole("heading", { name: "真实 API 会话", level: 1 })).toBeInTheDocument();
    expect(screen.queryByText("正在启动归档工作台")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("服务端访问令牌")).not.toBeInTheDocument();
    expect(window.location.hash).toBe("");
    expect(window.location.search).toBe("?view=archive");
    expect(storageSpy).not.toHaveBeenCalled();
  });

  it("returns to manual connection after automatic authentication fails and allows retry", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({}, { status: 401 })));
    window.history.replaceState(null, "", `/#token=${accessToken}`);
    render(<App />);

    expect(await screen.findByLabelText("服务端访问令牌")).toBeInTheDocument();
    expect(screen.queryByText("正在启动归档工作台")).not.toBeInTheDocument();
    expect(window.location.hash).toBe("");
    installApiMock();
    await connect();
  });

  it("shows manual connection immediately for an invalid startup token", () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    window.history.replaceState(null, "", "/#token=short");
    render(<App />);

    expect(screen.getByLabelText("服务端访问令牌")).toBeInTheDocument();
    expect(screen.queryByText("正在启动归档工作台")).not.toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("loads conversations and messages from authenticated APIs without persisting the token", async () => {
    installApiMock();
    const storageSpy = vi.spyOn(Storage.prototype, "setItem");
    render(<App />);
    expect(screen.getByText("工作台")).toBeInTheDocument();
    await connect();
    expect(await screen.findByText("来自服务端归档的数据")).toBeInTheDocument();
    expect(screen.getAllByText(/已归档 1 条消息/).length).toBeGreaterThan(0);
    expect(storageSpy).not.toHaveBeenCalled();
  });

  it("loads the next message page instead of keeping the first fixed slice", async () => {
    const messageRequests: string[] = [];
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/archive/summary")) return Response.json({ conversation_count: 1, message_count: 201, media_count: 0, revision: 1, local_collection_available: false });
      if (url.includes("/conversations") && !url.includes("/messages")) return Response.json([{
        conversation_id: "S:person-a_person-b",
        display_name: "人员甲、人员乙",
        // A legacy archive may carry a stale group type; the S: identifier must win.
        conversation_type: "group",
        participant_names: ["人员甲", "人员乙"],
        last_message_at: "2026-09-07T10:24:00Z",
        message_count: 201,
        media_count: 0,
        participant_count: 2,
      }]);
      if (url.includes("/messages/count")) return Response.json({ total: 201 });
      if (url.includes("/messages")) {
        messageRequests.push(url);
        const offset = Number(new URL(url, "http://localhost").searchParams.get("offset") || 0);
        const limit = Number(new URL(url, "http://localhost").searchParams.get("limit") || 200);
        const count = Math.min(201 - offset, limit);
        return Response.json(Array.from({ length: count }, (_, index) => ({
          stable_message_id: `message-${offset + index}`,
          conversation_id: "S:person-a_person-b",
          sender_id: "person-a",
          sender_name: "人员甲",
          sent_at: `2026-09-07T10:${String((offset + index) % 60).padStart(2, "0")}:00Z`,
          direction: "incoming",
          message_type: "text",
          body_text: `消息 ${offset + index}`,
          lifecycle: "active",
          media: [],
          raw_type: "text",
        })));
      }
      if (url.includes("/participants")) return Response.json([]);
      return Response.json({}, { status: 404 });
    }));
    render(<App />);
    fireEvent.change(screen.getByLabelText("服务端访问令牌"), { target: { value: accessToken } });
    fireEvent.click(screen.getByRole("button", { name: "验证并进入工作台" }));
    await screen.findByRole("heading", { name: "人员甲、人员乙", level: 1 });
    expect(screen.getAllByText("私人会话").length).toBeGreaterThan(0);
    expect(await screen.findByText(/共 201 条消息/)).toBeInTheDocument();
    expect(screen.getByLabelText("跳转页码")).toHaveValue("1");
    expect(screen.getByText((_, element) => element instanceof HTMLElement && element.classList.contains("page-jump") && element.textContent.includes("/ 2 页"))).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(messageRequests.some((url) => new URL(url, "http://localhost").searchParams.get("offset") === "200")).toBe(true));
    expect(await screen.findByText((_, element) => element instanceof HTMLElement && element.classList.contains("page-jump") && element.textContent.includes("/ 2 页"))).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "上一页" })).not.toBeDisabled();
    fireEvent.change(screen.getByLabelText("每页数量"), { target: { value: "100" } });
    await waitFor(() => expect(messageRequests.some((url) => new URL(url, "http://localhost").searchParams.get("limit") === "100" && new URL(url, "http://localhost").searchParams.get("offset") === "0")).toBe(true));
    expect(await screen.findByText((_, element) => element instanceof HTMLElement && element.classList.contains("page-jump") && element.textContent.includes("/ 3 页"))).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("跳转页码"), { target: { value: "99" } });
    fireEvent.blur(screen.getByLabelText("跳转页码"));
    await waitFor(() => expect(messageRequests.some((url) => new URL(url, "http://localhost").searchParams.get("offset") === "200" && new URL(url, "http://localhost").searchParams.get("limit") === "100")).toBe(true));
    expect(await screen.findByText((_, element) => element instanceof HTMLElement && element.classList.contains("page-jump") && element.textContent.includes("/ 3 页"))).toBeInTheDocument();
  });

  it("can collapse and expand the conversation detail panel", async () => {
    installApiMock();
    render(<App />);
    await connect();
    expect(screen.getByRole("button", { name: "收起会话详情" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "收起会话详情" }));
    expect(screen.getByRole("button", { name: "展开会话详情" })).toBeInTheDocument();
    expect(screen.queryByText("导出范围")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "展开会话详情" }));
    expect(screen.getByText("群成员")).toBeInTheDocument();
  });

  it("keeps collector configuration in one card and exposes compact controls", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "采集端" }));
    expect(await screen.findByLabelText("员工告知内容")).toBeInTheDocument();
    expect(screen.getByDisplayValue("加密传输本机企业微信聊天记录到服务端")).toBeInTheDocument();
    expect(screen.queryByText("采集前向员工展示，请清晰说明用途和范围。")).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "采集端配置" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "生成采集端" })).toBeInTheDocument();
    expect(screen.getByLabelText("加密密钥：")).toHaveValue("key-1");
    expect(screen.getByLabelText("访问令牌：")).toHaveValue(accessToken);
    expect(screen.getByRole("button", { name: "重新生成加密密钥" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "重新生成服务端访问令牌" })).toBeInTheDocument();
    expect(screen.getByLabelText("支持离线导出")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "生成采集端" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "保存配置" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "导出记录" })).not.toBeInTheDocument();
    expect(screen.queryByText("企微归档")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "会话" }));
    expect(screen.getByLabelText("导入会话")).toBeInTheDocument();
  });

  it("imports an encrypted package from the conversation page", async () => {
    installApiMock();
    render(<App />);
    await connect();
    const input = screen.getByLabelText("导入会话");
    expect(input).toHaveAttribute("accept", ".wca,.json");
    fireEvent.change(input, { target: { files: [new File(["wca"], "archive.wca", { type: "application/octet-stream" })] } });
    expect(await screen.findByText("会话导入完成：新增 2 条消息。"),).toBeInTheDocument();
  });

  it("collects the local client from the conversation toolbar and merges it into the archive", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "采集本机" }));
    expect(screen.getByRole("dialog", { name: "正在采集本机数据" })).toBeInTheDocument();
    expect(await screen.findByText("本机采集完成：新增 3 条，更新 1 条消息。")).toBeInTheDocument();
    expect(vi.mocked(fetch).mock.calls.some(([url]) => String(url).includes("/collections/local"))).toBe(true);
  });

  it("keeps a fast local export flow with format and redaction controls", async () => {
    installApiMock();
    render(<App />);
    await connect();
    const navigation = screen.getByRole("navigation", { name: "主导航" });
    expect([...navigation.querySelectorAll("button")].map((button) => button.textContent)).toEqual(["本机", "采集端", "会话", "设置"]);
    fireEvent.click(screen.getByRole("button", { name: "本机" }));
    expect(await screen.findByRole("heading", { name: "快速导出本机记录", level: 2 })).toBeInTheDocument();
    expect(screen.queryByText("本机数据")).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "本机导出", level: 1 })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "CSV" }));
    fireEvent.click(screen.getByLabelText("数据脱敏"));
    fireEvent.click(screen.getByRole("button", { name: "开始导出" }));
    expect(await screen.findByText(/本机导出完成：local-export.csv/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "打开导出目录" })).toBeInTheDocument();
  });

  it("imports a local JSON archive from the conversation page", async () => {
    installApiMock();
    render(<App />);
    await connect();
    const input = screen.getByLabelText("导入会话");
    fireEvent.change(input, { target: { files: [new File(["{}"], "local-export.json", { type: "application/json" })] } });
    expect(await screen.findByText("会话导入完成：新增 2 条消息。"),).toBeInTheDocument();
    expect(vi.mocked(fetch).mock.calls.some(([url]) => String(url).includes("/imports/json"))).toBe(true);
  });

  it("reloads the selected conversation when a collector upload changes the archive revision", async () => {
    let revision = 1;
    let messageRequests = 0;
    let poll: (() => Promise<unknown>) | undefined;
    const realSetInterval = window.setInterval.bind(window);
    vi.spyOn(window, "setInterval").mockImplementation(((handler: TimerHandler, timeout?: number, ...args: unknown[]) => {
      if (timeout === 5000) {
        poll = handler as () => Promise<unknown>;
        return 1;
      }
      return realSetInterval(handler, timeout, ...args);
    }) as typeof window.setInterval);
    vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/archive/summary")) return Response.json({ conversation_count: 1, message_count: revision, media_count: 0, revision, local_collection_available: false });
      if (url.includes("/conversations")) return Response.json([{ conversation_id: "conversation-1", display_name: "自动刷新会话", conversation_type: "group", last_message_at: "2026-09-15T10:00:00Z", message_count: revision, media_count: 0, participant_count: 3 }]);
      if (url.includes("/messages/count")) return Response.json({ total: revision });
      if (url.includes("/messages")) {
        messageRequests += 1;
        return Response.json([{ stable_message_id: `message-${revision}`, conversation_id: "conversation-1", sender_id: "participant-1", sent_at: "2026-09-15T10:00:00Z", direction: "incoming", message_type: "text", body_text: revision === 1 ? "刷新前消息" : "采集端上传后的消息", lifecycle: "active", media: [], raw_type: "text" }]);
      }
      if (url.includes("/participants")) return Response.json([]);
      return Response.json({}, { status: 404 });
    }));
    render(<App />);
    fireEvent.change(screen.getByLabelText("服务端访问令牌"), { target: { value: accessToken } });
    fireEvent.click(screen.getByRole("button", { name: "验证并进入工作台" }));
    await screen.findByRole("heading", { name: "自动刷新会话", level: 1 });
    expect(await screen.findByText("刷新前消息")).toBeInTheDocument();
    revision = 2;
    expect(poll).toBeDefined();
    await act(async () => { await poll?.(); });
    await waitFor(() => expect(messageRequests).toBeGreaterThan(1));
    expect(await screen.findByText("采集端上传后的消息")).toBeInTheDocument();
  });

  it("rejects imports outside the supported archive formats", async () => {
    installApiMock();
    render(<App />);
    await connect();
    const input = screen.getByLabelText("导入会话");
    fireEvent.change(input, { target: { files: [new File(["text"], "archive.txt", { type: "text/plain" })] } });
    expect(await screen.findByText("只支持导入采集端生成的 .wca 加密包或本机导出的 JSON。"),).toBeInTheDocument();
    expect(vi.mocked(fetch).mock.calls.some(([url]) => String(url).includes("/imports/enterprise"))).toBe(false);
  });

  it("saves a user-provided enterprise encryption key identifier", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "采集端" }));
    const customKey = "custom-enterprise-key-20260914";
    fireEvent.change(screen.getByLabelText("加密密钥："), { target: { value: customKey } });
    fireEvent.click(screen.getByRole("button", { name: "保存加密密钥" }));
    expect(await screen.findByDisplayValue(customKey)).toBeInTheDocument();
    expect(await screen.findByText("加密密钥已更新；新生成的采集端会使用该密钥。"),).toBeInTheDocument();
  });

  it("requires a second confirmation for entire archive export", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "导出" }));
    expect(screen.getByRole("checkbox", { name: "数据脱敏" })).not.toBeChecked();
    fireEvent.click(screen.getByRole("checkbox", { name: "数据脱敏" }));
    expect(screen.getByRole("checkbox", { name: "数据脱敏" })).toBeChecked();
    fireEvent.click(screen.getByText("全部档案"));
    fireEvent.click(screen.getByText("开始导出"));
    expect(screen.getByRole("heading", { name: "确认导出全部档案" })).toBeInTheDocument();
    expect(screen.getByText("单个文件")).toBeInTheDocument();
  });

  it("does not expose service shutdown to browser sessions", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    expect(await screen.findByRole("heading", { name: "关于" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "退出工作台" })).not.toBeInTheDocument();
  });
});
