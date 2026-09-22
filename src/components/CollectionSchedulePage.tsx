import { CalendarClock, HardDriveDownload, MonitorUp, Plus, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { createLocalCollectionPlan, deleteLocalCollectionPlan, getCollectionSchedules, updateLocalCollectionPlan, type CollectionSchedule, type CollectionSchedules, type LocalCollectionPlan } from "../lib/server-api";
import { CollectionScheduleFields, DEFAULT_COLLECTION_SCHEDULE } from "./CollectionScheduleFields";

const EMPTY_SCHEDULES: CollectionSchedules = { localPlans: [], collectorPlans: [] };
function formatTime(value?: string): string { if (!value) return "尚未执行"; const date = new Date(value); if (Number.isNaN(date.getTime())) return value; const pad = (part: number) => String(part).padStart(2, "0"); return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`; }

export function CollectionSchedulePage({ token }: { token: string }) {
  const [activeTab, setActiveTab] = useState<"collectors" | "local">("collectors");
  const [schedules, setSchedules] = useState<CollectionSchedules>(EMPTY_SCHEDULES);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [showAdd, setShowAdd] = useState(false);
  const [name, setName] = useState("本机采集计划");
  const [includeMedia, setIncludeMedia] = useState(false);
  const [draft, setDraft] = useState<CollectionSchedule>({ ...DEFAULT_COLLECTION_SCHEDULE, mode: "interval" });
  const [creating, setCreating] = useState(false);
  const requests = useRef(new Map<string, number>());

  useEffect(() => { let active = true; getCollectionSchedules(token).then((result) => { if (active) setSchedules(result); }).catch((reason) => { if (active) setError(reason instanceof Error ? reason.message : "无法读取采集计划。"); }).finally(() => { if (active) setLoading(false); }); return () => { active = false; }; }, [token]);

  const updatePlan = async (plan: LocalCollectionPlan, patch: Partial<Pick<LocalCollectionPlan, "name" | "includeMedia" | "schedule">>) => {
    const next = { ...plan, ...patch }; const requestId = (requests.current.get(plan.id) ?? 0) + 1; requests.current.set(plan.id, requestId);
    setSchedules((current) => ({ ...current, localPlans: current.localPlans.map((item) => item.id === plan.id ? next : item) })); setError("");
    try { const saved = await updateLocalCollectionPlan(token, plan.id, { name: next.name, includeMedia: next.includeMedia, schedule: next.schedule }); if (requests.current.get(plan.id) === requestId) setSchedules(saved); } catch (reason) { if (requests.current.get(plan.id) === requestId) setError(reason instanceof Error ? reason.message : "采集计划自动保存失败。"); }
  };

  const createPlan = async () => { if (!name.trim() || creating) return; setCreating(true); setError(""); try { setSchedules(await createLocalCollectionPlan(token, { name: name.trim(), includeMedia, schedule: draft })); setShowAdd(false); setName("本机采集计划"); setIncludeMedia(false); setDraft({ ...DEFAULT_COLLECTION_SCHEDULE, mode: "interval" }); } catch (reason) { setError(reason instanceof Error ? reason.message : "无法添加采集计划。"); } finally { setCreating(false); } };
  const removePlan = async (plan: LocalCollectionPlan) => { if (!window.confirm(`确定删除“${plan.name}”吗？`)) return; try { setSchedules(await deleteLocalCollectionPlan(token, plan.id)); } catch (reason) { setError(reason instanceof Error ? reason.message : "无法删除采集计划。"); } };
  const collectorPlans = schedules.collectorPlans.filter((plan) => plan.schedule.mode !== "disabled");

  return <main className="settings-page collection-schedule-page">
    <section className="settings-section schedule-workspace">
      <div className="schedule-toolbar">
        <div className="schedule-tabs" role="tablist" aria-label="采集计划类型">
          <button id="collector-schedule-tab" role="tab" aria-label="采集端" aria-controls="collector-schedule-panel" aria-selected={activeTab === "collectors"} className={activeTab === "collectors" ? "active" : undefined} type="button" onClick={() => setActiveTab("collectors")}><MonitorUp size={16} />采集端<span>{collectorPlans.length}</span></button>
          <button id="local-schedule-tab" role="tab" aria-label="本机" aria-controls="local-schedule-panel" aria-selected={activeTab === "local"} className={activeTab === "local" ? "active" : undefined} type="button" onClick={() => setActiveTab("local")}><HardDriveDownload size={16} />本机<span>{schedules.localPlans.length}</span></button>
        </div>
        {activeTab === "local" ? <button className="primary-button schedule-add-button" type="button" onClick={() => setShowAdd(true)}><Plus size={16} />添加本机计划</button> : null}
      </div>
      {error ? <p className="schedule-page-error">{error}</p> : null}
      {activeTab === "collectors" ? <div className="schedule-panel" id="collector-schedule-panel" role="tabpanel" aria-labelledby="collector-schedule-tab" aria-busy={loading}><div className="schedule-list">
        {collectorPlans.length === 0 ? <div className="schedule-empty">暂无启用持续采集的采集端。</div> : collectorPlans.map((plan) => <article className="schedule-row collector-plan-row" key={plan.collectorId}><span className="schedule-row-icon"><MonitorUp size={20} /></span><div className="schedule-row-copy"><strong>{plan.fileName || plan.collectorId}</strong><small>{plan.organizationName} · {plan.includeMedia ? "包含图片和文件" : "仅文本"}</small><small className="collector-plan-times">生成：{formatTime(plan.createdAt)} · 最近上传：{formatTime(plan.lastUploadAt)} · 预计下次：{formatTime(plan.nextRunAt)}</small></div><div className="collector-plan-schedule">{plan.schedule.mode === "daily" ? `每天 ${plan.schedule.dailyTime}` : `每 ${plan.schedule.intervalMinutes} 分钟`}</div><span className={plan.executableAvailable ? "collector-file-state available" : "collector-file-state"}>{plan.executableAvailable ? "文件可用" : "文件缺失"}</span></article>)}
      </div></div> : <div className="schedule-panel" id="local-schedule-panel" role="tabpanel" aria-labelledby="local-schedule-tab" aria-busy={loading}><div className="schedule-list">
        {schedules.localPlans.length === 0 ? <div className="schedule-empty">暂无本机采集计划，点击“添加本机计划”创建。</div> : schedules.localPlans.map((plan) => <article className="schedule-row local-plan-row" key={plan.id}><span className="schedule-row-icon"><HardDriveDownload size={20} /></span><div className="schedule-row-copy"><input aria-label={`计划名称 ${plan.name}`} className="schedule-name-input" value={plan.name} onChange={(event) => setSchedules((current) => ({ ...current, localPlans: current.localPlans.map((item) => item.id === plan.id ? { ...item, name: event.target.value } : item) }))} onBlur={(event) => { const value = event.target.value.trim(); if (value) void updatePlan(plan, { name: value }); }} /><label className="schedule-media-checkbox"><input type="checkbox" checked={plan.includeMedia} onChange={(event) => void updatePlan(plan, { includeMedia: event.target.checked })} />包含图片和文件</label><small>最近执行：{formatTime(plan.lastRunAt)}{plan.lastStatus === "error" ? ` · 失败：${plan.lastDetail || "未知错误"}` : plan.lastStatus === "success" ? " · 成功" : ""}</small><small>预计下次：{formatTime(plan.nextRunAt)}</small></div><CollectionScheduleFields idPrefix={`local-plan-${plan.id}`} schedule={plan.schedule} onChange={(schedule) => void updatePlan(plan, { schedule })} disabled={loading} /><button className="icon-danger-button" type="button" aria-label={`删除计划 ${plan.name}`} onClick={() => void removePlan(plan)}><Trash2 size={16} /></button></article>)}
      </div></div>}
    </section>
    {showAdd ? <div className="modal-backdrop" role="presentation" onMouseDown={() => setShowAdd(false)}><section className="modal-card schedule-add-dialog" role="dialog" aria-modal="true" aria-labelledby="schedule-add-title" onMouseDown={(event) => event.stopPropagation()}><button className="modal-close" type="button" aria-label="关闭添加计划" onClick={() => setShowAdd(false)}><X size={18} /></button><div className="schedule-add-heading"><span className="modal-icon"><CalendarClock /></span><h2 id="schedule-add-title">添加本机</h2></div><label className="settings-field"><span>计划名称</span><input aria-label="计划名称" value={name} maxLength={80} onChange={(event) => setName(event.target.value)} /></label><label className="local-collection-media-option"><input type="checkbox" checked={includeMedia} onChange={(event) => setIncludeMedia(event.target.checked)} />包含图片和文件</label><CollectionScheduleFields idPrefix="new-local-plan" schedule={draft} onChange={setDraft} disabled={creating} allowDisable={false} /><div className="modal-actions"><button className="primary-button" type="button" disabled={creating || !name.trim() || draft.mode === "disabled"} onClick={() => void createPlan()}>{creating ? "正在添加…" : "添加计划"}</button></div></section></div> : null}
  </main>;
}
