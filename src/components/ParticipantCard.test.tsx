import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ParticipantCard } from "./ParticipantCard";

describe("participant profiles", () => {
  it("shows profile fields in the expected sections and order and closes from the close button", () => {
    const onClose = vi.fn();
    render(<ParticipantCard participant={{ id: "synthetic-member", name: "测试成员", kind: "employee", profile: { mobile: "13800000000", phone: "010-12345678", office_phone: "010-00000000", email: "member@example.invalid", employee_no: "TEST-001", department: "测试企业/销售部/业务组", organization: "测试企业", position: "研发工程师", signature: "及时交付" } }} onClose={onClose} />);
    const element = screen.getByRole("dialog", { name: "测试成员" });
    const dialog = within(element);
    for (const value of ["手机", "13800000000", "电话", "010-12345678", "座机", "010-00000000", "邮箱", "member@example.invalid", "工号", "TEST-001", "部门", "测试企业/销售部/业务组", "测试企业", "研发工程师", "及时交付"]) {
      expect(dialog.getByText(value)).toBeInTheDocument();
    }
    expect(dialog.queryByText("职位")).not.toBeInTheDocument();
    expect(dialog.queryByText("地址")).not.toBeInTheDocument();
    expect(element.querySelector(".participant-profile-head")?.textContent).toContain("测试企业");
    expect(element.querySelector(".participant-profile-head")?.textContent).not.toContain("研发工程师");
    expect(element.querySelector("dl")?.textContent).toContain("职务研发工程师");
    expect(within(element.querySelector("dl")!).queryByText("测试企业", { exact: true })).not.toBeInTheDocument();
    const labels = ["部门", "职务", "邮箱", "工号", "手机", "电话", "座机", "归档标识", "身份"];
    const text = element.textContent || "";
    for (const [index, label] of labels.entries()) {
      expect(dialog.getByText(label)).toBeInTheDocument();
      if (index > 0) expect(text.indexOf(labels[index - 1])).toBeLessThan(text.indexOf(label));
    }
    fireEvent.click(dialog.getByRole("button", { name: "关闭人员详情" }));
    expect(onClose).toHaveBeenCalledOnce();
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
