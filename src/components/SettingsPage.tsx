import { Building2, Database, Download, LoaderCircle, Server, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import { getServerHealth, updateSuperAdmin, type ServerHealth } from "../lib/server-api";
import { checkForUpdate, installUpdate, type UpdateInfo } from "../lib/update";
import { isTauri } from "@tauri-apps/api/core";

interface SettingsPageProps {
  token: string;
  superAdminEnabled: boolean;
  onSuperAdminChange: (enabled: boolean) => void;
}

export function SettingsPage({ token, superAdminEnabled, onSuperAdminChange }: SettingsPageProps) {
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [health, setHealth] = useState<ServerHealth>();
  const [healthError, setHealthError] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>();
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateMessage, setUpdateMessage] = useState("");

  useEffect(() => {
    const controller = new AbortController();
    void getServerHealth(controller.signal).then((next) => {
      if (!controller.signal.aborted) setHealth(next);
    }).catch(() => { if (!controller.signal.aborted) setHealthError(true); });
    return () => controller.abort();
  }, []);

  const toggleSuperAdmin = async (enabled: boolean) => {
    if (saving) return;
    setSaving(true);
    setError("");
    try {
      const next = await updateSuperAdmin(token, enabled);
      onSuperAdminChange(next.superAdminEnabled);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "超管模式保存失败。");
    } finally {
      setSaving(false);
    }
  };

  const runUpdateCheck = async () => {
    if (updateBusy) return;
    setUpdateBusy(true);
    setUpdate(null);
    setUpdateMessage("");
    try {
      const next = await checkForUpdate();
      setUpdate(next);
      setUpdateMessage(next ? `发现新版本 ${next.latestVersion}。` : "当前已是最新版本。");
    } catch (reason) {
      setUpdateMessage(typeof reason === "string" ? reason : reason instanceof Error ? reason.message : "检查更新失败。");
    } finally {
      setUpdateBusy(false);
    }
  };

  const runUpdateInstall = async () => {
    if (!update || updateBusy) return;
    setUpdateBusy(true);
    setUpdateMessage("正在下载更新，程序将自动重启…");
    try {
      await installUpdate(update);
      setUpdateMessage("更新已校验，正在等待程序退出并重启…");
    } catch (reason) {
      setUpdateMessage(typeof reason === "string" ? reason : reason instanceof Error ? reason.message : "升级失败。");
      setUpdateBusy(false);
    }
  };

  return (
    <main className="settings-page">
      <div className="general-settings-grid">
        <section className="settings-section compact-setting">
          <div className="compact-heading"><ShieldCheck size={18} /><h2>访问权限</h2></div>
          <label className="settings-toggle-row">
            <span><strong>启用超管模式</strong><small>关闭时仅展示会话目录和已收集数量，不展示聊天内容、搜索和导出。</small></span>
            <input aria-label="启用超管模式" type="checkbox" checked={superAdminEnabled} disabled={saving} onChange={(event) => void toggleSuperAdmin(event.target.checked)} />
          </label>
          {saving && <p className="settings-inline-status"><LoaderCircle className="spin" size={14} />正在保存设置…</p>}
          {error && <p className="settings-error">{error}</p>}
        </section>

        <section className="settings-section compact-setting">
          <div className="compact-heading"><Database size={18} /><h2>归档存储</h2></div>
          <div className="setting-fact"><span>存储方式</span><strong>本机单节点归档库</strong></div>
          <div className="setting-fact"><span>数据目录</span><strong>serverData</strong></div>
          <p>支持全文检索、幂等导入、消息修订和完整性校验。</p>
        </section>

        <section className="settings-section compact-setting">
          <div className="compact-heading"><Server size={18} /><h2>网络与运行</h2></div>
          <div className="setting-fact"><span>服务监听地址</span><strong>{health?.listenAddress || (healthError ? "读取失败" : "读取中…")}</strong></div>
          <div className="setting-fact"><span>监听范围</span><strong className={health?.networkMode === "loopback_only" ? "safe-text" : "warning-text"}>{!health ? (healthError ? "读取失败" : "读取中…") : health.networkMode === "loopback_only" ? "仅限本机" : "已配置远程访问"}</strong></div>
          <p>点击桌面窗口右上角关闭按钮时会同步停止本机服务；浏览器访问不提供停服操作。</p>
        </section>

        <section className="settings-section compact-setting about-setting">
          <div className="compact-heading"><Building2 size={18} /><h2>关于</h2></div>
          <div className="setting-fact"><span>产品</span><strong>企业微信记录归档</strong></div>
          <div className="setting-fact"><span>版本</span><strong>{health?.version || (healthError ? "读取失败" : "读取中…")}</strong></div>
          {isTauri() ? <><p>点击检查将连接项目 Release；升级会保留数据并重启工作台。</p><div className="settings-update-actions"><button className="secondary-button" type="button" onClick={() => void runUpdateCheck()} disabled={updateBusy}>
            {updateBusy ? <LoaderCircle className="spin" size={14} /> : <Download size={14} />}
            检查更新
          </button>
          {update && <button className="primary-button" type="button" onClick={() => void runUpdateInstall()} disabled={updateBusy}>升级到 {update.latestVersion} 并重启</button>}</div></> : <p>请在桌面工作台中检查和安装更新。</p>}
          {updateMessage && <p role="status" className="settings-inline-status">{updateMessage}</p>}
        </section>
      </div>
    </main>
  );
}
