import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { CollectionScheduleFields } from "./CollectionScheduleFields";

it("allows replacing the entire interval value and accepts one-minute steps", () => {
  const onChange = vi.fn();
  render(<CollectionScheduleFields idPrefix="test" schedule={{ mode: "interval", intervalMinutes: 60, dailyTime: "02:00" }} onChange={onChange} />);
  expect(screen.queryByText("触发方式")).not.toBeInTheDocument();
  expect(screen.getByLabelText("按间隔")).toBeChecked();
  expect(screen.getByLabelText("每日定时")).not.toBeChecked();
  const input = screen.getByLabelText("间隔（分钟）");
  expect(input).toHaveAttribute("step", "1");
  expect(input).toHaveAttribute("min", "1");

  fireEvent.change(input, { target: { value: "" } });
  expect(input).toHaveValue(null);
  expect(onChange).not.toHaveBeenCalled();
  fireEvent.change(input, { target: { value: "17" } });
  expect(onChange).toHaveBeenLastCalledWith({ mode: "interval", intervalMinutes: 17, dailyTime: "02:00" });

  fireEvent.click(screen.getByLabelText("按间隔"));
  expect(onChange).toHaveBeenLastCalledWith({ mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" });
});

it("keeps the schedule value slot mounted while no mode is selected", () => {
  const { container, rerender } = render(<CollectionScheduleFields idPrefix="test" schedule={{ mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" }} onChange={vi.fn()} />);
  const slot = container.querySelector(".schedule-value-slot");

  expect(slot).toBeInTheDocument();
  expect(slot).toBeEmptyDOMElement();

  rerender(<CollectionScheduleFields idPrefix="test" schedule={{ mode: "daily", intervalMinutes: 60, dailyTime: "02:00" }} onChange={vi.fn()} />);
  expect(container.querySelector(".schedule-value-slot")).toBe(slot);
  expect(screen.getByLabelText("每天时间")).toBeInTheDocument();
});
