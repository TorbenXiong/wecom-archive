import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ParticipantCard } from "./ParticipantCard";

describe("participant profiles", () => {
  it("shows configured contact and organization fields and closes from the close button", () => {
    const onClose = vi.fn();
    render(<ParticipantCard participant={{ id: "synthetic-member", name: "测试成员", kind: "employee", profile: { mobile: "13800000000", office_phone: "010-00000000", email: "member@example.invalid", employee_no: "TEST-001", department: "测试企业/销售部/业务组", organization: "测试企业" } }} onClose={onClose} />);
    const dialog = within(screen.getByRole("dialog", { name: "测试成员" }));
    for (const value of ["手机", "13800000000", "座机", "010-00000000", "邮箱", "member@example.invalid", "工号", "TEST-001", "部门", "测试企业/销售部/业务组", "测试企业"]) {
      expect(dialog.getByText(value)).toBeInTheDocument();
    }
    expect(dialog.queryByText("职位")).not.toBeInTheDocument();
    expect(dialog.queryByText("地址")).not.toBeInTheDocument();
    fireEvent.click(dialog.getByRole("button", { name: "关闭人员详情" }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("puts enterprise and signature below the name and position in the details", () => {
    render(<ParticipantCard participant={{ id: "member-with-profile", name: "成员甲", profile: { organization: "测试企业", position: "研发工程师", signature: "及时交付", mobile: "13800000000" } }} onClose={vi.fn()} />);
    const dialog = screen.getByRole("dialog", { name: "成员甲" });
    expect(within(dialog).getByText("研发工程师")).toBeInTheDocument();
    expect(within(dialog).getByText("及时交付")).toBeInTheDocument();
    expect(dialog.querySelector(".participant-profile-head")?.textContent).toContain("测试企业");
    expect(dialog.querySelector(".participant-profile-head")?.textContent).not.toContain("研发工程师");
    expect(dialog.querySelector("dl")?.textContent).toContain("职务研发工程师");
    expect(dialog.querySelector("dl")?.textContent).not.toContain("测试企业");
  });

  it("puts contact numbers below employee number and preserves the remaining order", () => {
    render(<ParticipantCard participant={{ id: "member-order", name: "成员乙", kind: "employee", profile: { mobile: "13800000000", phone: "010-12345678", office_phone: "1001", department: "研发部", organization: "测试企业", position: "研发工程师", email: "member@example.invalid", employee_no: "TEST-002" } }} onClose={vi.fn()} />);
    const text = screen.getByRole("dialog", { name: "成员乙" }).textContent || "";
    expect(text.indexOf("部门")).toBeLessThan(text.indexOf("职务"));
    expect(text.indexOf("职务")).toBeLessThan(text.indexOf("邮箱"));
    expect(text.indexOf("邮箱")).toBeLessThan(text.indexOf("工号"));
    expect(text.indexOf("工号")).toBeLessThan(text.indexOf("手机"));
    expect(text.indexOf("手机")).toBeLessThan(text.indexOf("电话"));
    expect(text.indexOf("电话")).toBeLessThan(text.indexOf("座机"));
    expect(text.indexOf("座机")).toBeLessThan(text.indexOf("归档标识"));
    expect(text.indexOf("归档标识")).toBeLessThan(text.indexOf("身份"));
  });

  it("keeps old participants readable without inventing missing contact details", () => {
    render(<ParticipantCard participant={{ id: "legacy-member", name: "历史成员" }} onClose={vi.fn()} />);
    expect(screen.getByRole("dialog", { name: "历史成员" })).toBeInTheDocument();
    expect(screen.getByText("归档标识")).toBeInTheDocument();
    for (const label of ["手机", "座机", "邮箱", "工号", "部门", "企业", "职务"]) {
      expect(screen.queryByText(label)).not.toBeInTheDocument();
    }
  });
  it("shows enterprise-defined fields as escaped plain text", () => {
    render(<ParticipantCard participant={{ id: "synthetic-member", name: "测试成员", profile: { "custom:办公地点": "测试楼A栋", "custom:备注": "<script>synthetic</script>" } }} onClose={vi.fn()} />);
    expect(screen.getByText("办公地点")).toBeInTheDocument();
    expect(screen.getByText("测试楼A栋")).toBeInTheDocument();
    expect(screen.getByText("<script>synthetic</script>")).toBeInTheDocument();
    expect(document.querySelector(".participant-profile-card script")).toBeNull();
  });
});
