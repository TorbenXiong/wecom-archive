import type { CollectionSchedule, CollectionScheduleMode } from "../lib/server-api";
import { useEffect, useState } from "react";

interface CollectionScheduleFieldsProps {
  schedule: CollectionSchedule;
  onChange: (schedule: CollectionSchedule) => void;
  disabled?: boolean;
  allowDisable?: boolean;
  idPrefix: string;
}

export const DEFAULT_COLLECTION_SCHEDULE: CollectionSchedule = {
  mode: "disabled",
  intervalMinutes: 60,
  dailyTime: "02:00",
};

export function CollectionScheduleFields({ schedule, onChange, disabled, allowDisable = true, idPrefix }: CollectionScheduleFieldsProps) {
  const [intervalDraft, setIntervalDraft] = useState(String(schedule.intervalMinutes));
  useEffect(() => setIntervalDraft(String(schedule.intervalMinutes)), [schedule.intervalMinutes]);
  const update = (patch: Partial<CollectionSchedule>) => onChange({ ...schedule, ...patch });
  return <div className="schedule-controls">
    <div className="schedule-mode-options" role="radiogroup" aria-label="采集计划">
      {(["interval", "daily"] as CollectionScheduleMode[]).map((mode) => <label className="schedule-mode-option" key={mode}>
        <input
          id={`${idPrefix}-mode-${mode}`}
          name={`${idPrefix}-mode`}
          type="radio"
          disabled={disabled}
          checked={schedule.mode === mode}
          onClick={() => { if (allowDisable && schedule.mode === mode) update({ mode: "disabled" }); }}
          onChange={() => update({ mode })}
        />
        <span>{mode === "interval" ? "按间隔" : "每日定时"}</span>
      </label>)}
    </div>
    <div className="schedule-value-slot">
      {schedule.mode === "interval" && <label htmlFor={`${idPrefix}-interval`}><span>间隔（分钟）</span><input id={`${idPrefix}-interval`} disabled={disabled} type="number" min={1} max={10080} step={1} value={intervalDraft} onChange={(event) => { const value = event.target.value; setIntervalDraft(value); const parsed = Number(value); if (value !== "" && Number.isInteger(parsed)) update({ intervalMinutes: Math.min(10080, Math.max(1, parsed)) }); }} onBlur={() => { const parsed = Number(intervalDraft); const normalized = Number.isInteger(parsed) ? Math.min(10080, Math.max(1, parsed)) : 1; setIntervalDraft(String(normalized)); update({ intervalMinutes: normalized }); }} /></label>}
      {schedule.mode === "daily" && <label htmlFor={`${idPrefix}-time`}><span>每天时间</span><input id={`${idPrefix}-time`} disabled={disabled} type="time" value={schedule.dailyTime} onChange={(event) => update({ dailyTime: event.target.value })} /></label>}
    </div>
  </div>;
}
