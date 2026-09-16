import { describe, expect, it } from "vitest";
import type { MessageItem } from "../domain/types";
import { extractGroupInfo } from "./DetailPanel";

function systemMessage(id: string, body: string, rawType?: string): MessageItem {
  return {
    id,
    conversationId: "R:group",
    senderId: "system",
    senderName: "系统",
    senderInitial: "系",
    sentAt: "2026-09-15T00:00:00Z",
    timeLabel: "08:00",
    direction: "system",
    type: "system",
    rawType,
    body,
    lifecycle: "active",
  };
}

describe("group detail metadata", () => {
  it("does not turn opaque system identifiers into announcements or boards", () => {
    expect(extractGroupInfo([systemMessage("1", "1688856080881486")])).toEqual({
      announcement: undefined,
      board: undefined,
    });
  });

  it("shows a real multiline announcement and removes an adjacent identifier", () => {
    const text = "1688856080881486\n各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称";
    expect(extractGroupInfo([systemMessage("1", text)]).announcement).toBe(
      "各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称",
    );
  });

  it("only returns a board when a board message is explicitly identified", () => {
    expect(extractGroupInfo([systemMessage("1", "群看板：本周完成上线准备")])).toEqual({
      announcement: undefined,
      board: "本周完成上线准备",
    });
  });

  it("uses collected group metadata even when the announcement is short", () => {
    expect(extractGroupInfo([systemMessage("1", "明天放假", "group_announcement")]).announcement).toBe("明天放假");
  });
});
