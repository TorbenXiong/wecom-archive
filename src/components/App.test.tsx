import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import App from "../App";

const accessToken = "0123456789abcdefghijklmnop";

function installApiMock() {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/archive/summary")) {
      return Response.json({ conversation_count: 1, message_count: 1, media_count: 0 });
    }
    if (url.includes("/conversations")) {
      return Response.json([{
        conversation_id: "conversation-1",
        display_name: "真实 API 会话",
        conversation_type: "direct",
        last_message_at: "2026-09-07T10:24:00Z",
        message_count: 1,
        media_count: 0,
        participant_count: 1,
      }]);
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
      return Response.json({ configured: false, organizationId: "org-1", organizationName: "", collectionNotice: "加密传输本机企业微信聊天记录到服务端", keyId: "key-1" });
    }
    return Response.json({}, { status: 404 });
  }));
}

async function connect() {
  fireEvent.change(screen.getByLabelText("服务端访问令牌"), { target: { value: accessToken } });
  fireEvent.click(screen.getByRole("button", { name: "验证并进入工作台" }));
  await screen.findByRole("heading", { name: "真实 API 会话", level: 1 });
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("archive workspace", () => {
  it("loads conversations and messages from authenticated APIs without persisting the token", async () => {
    installApiMock();
    const storageSpy = vi.spyOn(Storage.prototype, "setItem");
    render(<App />);
    expect(screen.getByText("企业版")).toBeInTheDocument();
    await connect();
    expect(await screen.findByText("来自服务端归档的数据")).toBeInTheDocument();
    expect(screen.getAllByText(/已归档 1 条消息/).length).toBeGreaterThan(0);
    expect(storageSpy).not.toHaveBeenCalled();
  });

  it("separates server configuration and exposes key and token controls", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "服务端配置" }));
    expect(await screen.findByLabelText("员工告知内容")).toBeInTheDocument();
    expect(screen.getByDisplayValue("加密传输本机企业微信聊天记录到服务端")).toBeInTheDocument();
    expect(screen.queryByText("采集前向员工展示，请清晰说明用途和范围。")).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "采集端配置" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "生成采集端" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "重新生成密钥" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "随机重新生成" })).toBeInTheDocument();
    expect(screen.getByDisplayValue(accessToken)).toBeInTheDocument();
    expect(screen.getByText("导入加密包")).toBeInTheDocument();
  });

  it("requires a second confirmation for entire archive export", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByText("全部档案"));
    fireEvent.click(screen.getByText("开始导出"));
    expect(screen.getByRole("heading", { name: "确认导出全部档案" })).toBeInTheDocument();
    expect(screen.getByText("ZIP 包")).toBeInTheDocument();
  });

  it("does not expose service shutdown to browser sessions", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    expect(await screen.findByRole("heading", { name: "关于" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "退出企业版" })).not.toBeInTheDocument();
  });
});
