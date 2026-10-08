import { HardDriveDownload, MonitorUp, Pencil, RotateCcw, Save, Search } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { createLocalCollectionPlan, getCollectionTargets, getEnterpriseConfig, requestCollectorCollection, updateCollector, updateEnterpriseConfig, updateLocalCollectionPlan, type CollectionSchedule, type CollectionTarget } from "../lib/server-api";
import { CollectionScheduleFields, DEFAULT_COLLECTION_SCHEDULE } from "./CollectionScheduleFields";

function formatTime(value: string | undefined, fallback = "尚未执行"): string {
  if (!value) return fallback;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN", { hour12: false });
}

function scheduleLabel(schedule: CollectionSchedule): string {
  if (schedule.mode === "disabled") return "已停用";
  return schedule.mode === "daily" ? `每天 ${schedule.dailyTime}` : `每 ${schedule.intervalMinutes} 分钟`;
}

function targetDisplayName(target: CollectionTarget): string {
  if (target.kind === "server") return "本机";
  const defaultName = !target.displayName || target.displayName.startsWith("采集端 ");
  return defaultName ? target.clientIp || target.displayName || "采集端" : target.displayName;
}

function scheduleExecutionLabel(schedule?: CollectionSchedule): string | undefined {
  if (!schedule) return undefined;
  if (schedule.mode === "daily") return `每日 ${schedule.dailyTime} 执行`;
  if (schedule.mode === "interval") return `每 ${schedule.intervalMinutes} 分钟执行`;
  return "已停用";
}

interface CollectorDraft {
  targetId: string;
  kind: CollectionTarget["kind"];
  displayName: string;
  enabled: boolean;
  includeMedia: boolean;
  dataRedaction: boolean;
  offlineExportEnabled: boolean;
  hiddenModeEnabled: boolean;
  collectorLoggingEnabled: boolean;
  planName: string;
  schedule: CollectionSchedule;
}

function draftFromTarget(target: CollectionTarget): CollectorDraft {
  const plan = target.plans[0];
  return {
    targetId: target.targetId,
    kind: target.kind,
    displayName: target.kind === "server" ? "本机" : target.displayName,
    enabled: target.status !== "disabled" && target.status !== "revoked",
    includeMedia: plan?.includeMedia ?? target.includeMedia ?? false,
    dataRedaction: target.dataRedaction ?? true,
    offlineExportEnabled: target.offlineExportEnabled ?? false,
    hiddenModeEnabled: target.hiddenModeEnabled ?? false,
    collectorLoggingEnabled: target.collectorLogLevel != null && target.collectorLogLevel !== "off",
    planName: plan?.name || (target.kind === "server" ? "本机采集计划" : "采集计划"),
    schedule: plan?.schedule ?? { ...DEFAULT_COLLECTION_SCHEDULE },
  };
}

function draftChanged(target: CollectionTarget, draft: CollectorDraft): boolean {
  const baseline = draftFromTarget(target);
  return JSON.stringify(baseline) !== JSON.stringify(draft);
}

interface CollectionSchedulePageProps {
  token: string;
  localCollectionAvailable?: boolean;
  collectingLocal?: boolean;
  onOpenLocalCollection?: () => void;
}

export function CollectionSchedulePage({ token, localCollectionAvailable = false, collectingLocal = false, onOpenLocalCollection }: CollectionSchedulePageProps) {
  const [targets, setTargets] = useState<CollectionTarget[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [search, setSearch] = useState("");
  const [editMode, setEditMode] = useState(false);
  const [drafts, setDrafts] = useState<Record<string, CollectorDraft>>({});
  const [saving, setSaving] = useState(false);
  const targetsRef = useRef<CollectionTarget[]>([]);
  const editModeRef = useRef(false);
  const fetchingRef = useRef(false);

  const refresh = useCallback(async (background = false) => {
    if (fetchingRef.current) return;
    fetchingRef.current = true;
    if (!background) setLoading(true);
    try {
      const nextTargets = (await getCollectionTargets(token)).targets ?? [];
      const changed = JSON.stringify(targetsRef.current) !== JSON.stringify(nextTargets);
      targetsRef.current = nextTargets;
      if (changed) {
        setTargets(nextTargets);
        if (editModeRef.current) setDrafts((current) => Object.fromEntries(nextTargets.map((target) => [target.targetId, current[target.targetId] ?? draftFromTarget(target)])));
      }
      setError("");
    } catch (reason) {
      if (!background) setError(reason instanceof Error ? reason.message : "无法读取采集节点。");
    } finally {
      fetchingRef.current = false;
      if (!background) setLoading(false);
    }
  }, [token]);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => refresh(true), 5000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  const normalizedSearch = search.trim().toLocaleLowerCase();
  const visibleTargets = targets.filter((target) => !normalizedSearch || [target.displayName, target.clientIp, target.targetId, target.logDirectory, target.kind === "server" ? "服务端" : "远程采集端", ...target.plans.map((plan) => `${plan.name} ${scheduleLabel(plan.schedule)}`)].join(" ").toLocaleLowerCase().includes(normalizedSearch));
  const dirty = targets.some((target) => drafts[target.targetId] && draftChanged(target, drafts[target.targetId]));

  const updateDraft = (targetId: string, patch: Partial<CollectorDraft>) => {
    setDrafts((current) => ({ ...current, [targetId]: { ...current[targetId], ...patch } }));
  };

  const startEditing = () => {
    setDrafts(Object.fromEntries(targets.map((target) => [target.targetId, draftFromTarget(target)])));
    editModeRef.current = true;
    setEditMode(true);
    setError("");
  };

  const restore = () => {
    setDrafts(Object.fromEntries(targets.map((target) => [target.targetId, draftFromTarget(target)])));
    editModeRef.current = false;
    setEditMode(false);
    setError("");
  };

  const save = async () => {
    const changedTargets = targets.filter((target) => drafts[target.targetId] && draftChanged(target, drafts[target.targetId]));
    if (saving || changedTargets.length === 0) return;
    if (changedTargets.some((target) => !drafts[target.targetId].displayName.trim())) {
      setError("采集端名称不能为空。");
      return;
    }
    setSaving(true);
    setError("");
    try {
      for (const target of changedTargets) {
        const draft = drafts[target.targetId];
        if (target.kind === "server") {
          if (draft.dataRedaction !== target.dataRedaction) {
            const config = await getEnterpriseConfig(token);
            await updateEnterpriseConfig(token, {
              organizationName: config.organizationName,
              uploadUrl: config.uploadUrl,
              keyId: config.keyId || undefined,
              includeMedia: config.includeMedia,
              dataRedaction: draft.dataRedaction,
            });
          }
          const plan = target.plans[0];
          if (!plan || draft.includeMedia !== plan.includeMedia || JSON.stringify(draft.schedule) !== JSON.stringify(plan.schedule)) {
            const input = { name: draft.planName, includeMedia: draft.includeMedia, schedule: draft.schedule };
            if (plan) await updateLocalCollectionPlan(token, plan.id, input);
            else await createLocalCollectionPlan(token, input);
          }
        } else {
          await updateCollector(token, target.targetId, {
            displayName: draft.displayName.trim(),
            enabled: draft.enabled,
            includeMedia: draft.includeMedia,
            dataRedaction: draft.dataRedaction,
            offlineExportEnabled: draft.offlineExportEnabled,
            hiddenModeEnabled: draft.hiddenModeEnabled,
            collectorLogLevel: draft.collectorLoggingEnabled ? "normal" : "off",
            schedule: draft.schedule,
          });
        }
      }
      editModeRef.current = false;
      setEditMode(false);
      setDrafts({});
      await refresh();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "采集端配置保存失败。已保存的节点不会自动回滚，请重新进入编辑后核对。");
    } finally {
      setSaving(false);
    }
  };

  const collectRemote = async (target: CollectionTarget) => {
    try {
      await requestCollectorCollection(token, target.targetId);
      setError(`已向“${target.displayName}”发送采集请求，设备下次心跳时执行。`);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "远程采集请求发送失败。");
    }
  };

  const content = <>
    <section className="settings-section schedule-workspace">
      <div className="schedule-list-tools collector-list-tools">
        <label className="schedule-search"><Search size={16} /><input aria-label="检索采集端" placeholder="检索采集端" value={search} onChange={(event) => setSearch(event.target.value)} /></label>
        <div className="schedule-toolbar-actions">
          {!editMode && <label className="secondary-button collector-edit-toggle"><input type="checkbox" role="switch" aria-label="编辑模式" checked={editMode} onChange={() => startEditing()} disabled={saving} /><Pencil size={16} /><span>编辑</span></label>}
          {editMode && <><button className="secondary-button" type="button" onClick={restore} disabled={saving}><RotateCcw size={16} />还原</button><button className="primary-button" type="button" onClick={() => void save()} disabled={saving || !dirty}><Save size={16} />{saving ? "保存中…" : "保存"}</button></>}
        </div>
      </div>
      {error ? <p className="schedule-page-error">{error}</p> : null}
      <div className="schedule-target-list" aria-busy={loading}>
        {visibleTargets.filter((target) => target.kind === "server").map((target) => <TargetSection key={target.targetId} target={target} draft={editMode ? drafts[target.targetId] : undefined} onDraftChange={(patch) => updateDraft(target.targetId, patch)} onCollect={onOpenLocalCollection} collectDisabled={!localCollectionAvailable || collectingLocal || editMode} collecting={collectingLocal} />)}
        {visibleTargets.filter((target) => target.kind === "collector").map((target) => <TargetSection key={target.targetId} target={target} draft={editMode ? drafts[target.targetId] : undefined} onDraftChange={(patch) => updateDraft(target.targetId, patch)} onCollect={() => void collectRemote(target)} collectDisabled={editMode} />)}
        {!loading && visibleTargets.length === 0 ? <div className="schedule-empty">{normalizedSearch ? "没有匹配的采集端或计划。" : "暂无采集节点。"}</div> : null}
      </div>
    </section>
  </>;
  return <div className="collector-management-panel">{content}</div>;
}

function TargetSection({ target, draft, onDraftChange, onCollect, collectDisabled, collecting }: { target: CollectionTarget; draft?: CollectorDraft; onDraftChange: (patch: Partial<CollectorDraft>) => void; onCollect?: () => void; collectDisabled?: boolean; collecting?: boolean }) {
  const statusLabel = target.status === "online" ? "在线" : target.status === "offline" ? "离线" : target.status === "disabled" ? "已停用" : "已吊销";
  const plan = target.plans[0];
  const scheduleText = scheduleExecutionLabel(plan?.schedule);
  return <section className={`schedule-target-section${draft ? " editing" : ""}`}>
    <header className="schedule-target-heading">
      <span className="schedule-row-icon">{target.kind === "server" ? <HardDriveDownload size={20} /> : <MonitorUp size={20} />}</span>
      <div className="collector-target-details">
        <div className="collector-target-identity">
          {draft && target.kind === "collector" ? <input className="collector-inline-name" aria-label={`名称：${targetDisplayName(target)}`} maxLength={80} value={draft.displayName} onChange={(event) => onDraftChange({ displayName: event.target.value })} /> : <strong>{targetDisplayName(target)}</strong>}
          {draft ? <div className="collector-inline-options"><label><input type="checkbox" aria-label={`含文件和图片：${targetDisplayName(target)}`} checked={draft.includeMedia} onChange={(event) => onDraftChange({ includeMedia: event.target.checked })} />含文件和图片</label><label><input type="checkbox" aria-label={`数据脱敏：${targetDisplayName(target)}`} checked={draft.dataRedaction} onChange={(event) => onDraftChange({ dataRedaction: event.target.checked })} />数据脱敏</label>{target.kind === "collector" && <><label><input type="checkbox" aria-label={`支持离线导出：${targetDisplayName(target)}`} checked={draft.offlineExportEnabled} onChange={(event) => onDraftChange({ offlineExportEnabled: event.target.checked })} />支持离线导出</label><label><input type="checkbox" aria-label={`隐藏式采集端：${targetDisplayName(target)}`} checked={draft.hiddenModeEnabled} onChange={(event) => onDraftChange({ hiddenModeEnabled: event.target.checked })} />隐藏式采集端</label><label><input type="checkbox" aria-label={`开启采集端日志：${targetDisplayName(target)}`} checked={draft.collectorLoggingEnabled} onChange={(event) => onDraftChange({ collectorLoggingEnabled: event.target.checked })} />开启采集端日志</label></>}</div> : <div className="collector-target-summary"><small>{target.kind === "collector" ? <>IP：{target.clientIp || "未知"}　心跳：{formatTime(target.lastSeenAt)}{target.clientVersion ? `　版本：${target.clientVersion}` : ""}　</> : null}{(plan?.includeMedia ?? target.includeMedia) ? "全内容" : "仅文本"}　{target.dataRedaction ? "脱敏" : "不脱敏"}{target.kind === "collector" ? <>　{target.offlineExportEnabled ? "支持" : "不支持"}离线导出　{target.hiddenModeEnabled ? "隐藏运行" : "显示运行"}　日志：{target.collectorLogLevel && target.collectorLogLevel !== "off" ? "已开启" : "未开启"}</> : null}</small><small>采集计划：{scheduleText ?? "已停用"}　最近成功：{formatTime(target.kind === "collector" ? plan?.lastUploadAt : plan?.lastRunAt, "尚未成功")}　下次执行：{formatTime(plan?.nextRunAt)}</small>{target.kind === "collector" ? <small>实例标识：{target.targetId}{target.logDirectory ? `　服务端日志目录：${target.logDirectory}/日期.log` : ""}</small> : null}</div>}
          {target.lastError ? <small className="schedule-error-detail">最近错误：{target.lastError}</small> : null}
        </div>
        {draft ? <div className="collector-inline-plan"><span>采集计划</span><CollectionScheduleFields idPrefix={`inline-${target.targetId}`} schedule={draft.schedule} onChange={(schedule) => onDraftChange({ schedule })} /></div> : null}
      </div>
      <span className={`collector-status ${target.status}`}>{statusLabel}</span>
      <div className="schedule-target-actions"><button className="primary-button" type="button" onClick={onCollect} disabled={!onCollect || collectDisabled}><HardDriveDownload size={15} />{collecting ? "采集中…" : "采集"}</button></div>
    </header>
  </section>;
}
