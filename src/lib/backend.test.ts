import { describe, expect, it } from "vitest";
import { formatBackendError } from "./backend";

describe("formatBackendError", () => {
  it("keeps a safe Tauri error code when the rejection is a plain object", () => {
    expect(formatBackendError({
      code: "SOURCE_KEY_INVALID",
      message: "密钥无法打开加密数据库；未读取或导出任何消息。",
      recoverable: true,
    }, "采集失败，诊断信息已脱敏。"))
      .toContain("错误码：SOURCE_KEY_INVALID");
  });

  it("parses JSON-stringified command errors", () => {
    expect(formatBackendError(JSON.stringify({ code: "SOURCE_SCHEMA_UNSUPPORTED", message: "结构不匹配" }), "失败"))
      .toBe("结构不匹配（错误码：SOURCE_SCHEMA_UNSUPPORTED）");
  });

  it("uses the fallback for opaque rejections", () => {
    expect(formatBackendError({ unexpected: true }, "采集失败，诊断信息已脱敏。"))
      .toBe("采集失败，诊断信息已脱敏。");
  });
});
