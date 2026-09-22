import { Building2, Database, LoaderCircle, Server, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { updateSuperAdmin } from "../lib/server-api";

interface SettingsPageProps {
  token: string;
  superAdminEnabled: boolean;
  onSuperAdminChange: (enabled: boolean) => void;
}

export function SettingsPage({ token, superAdminEnabled, onSuperAdminChange }: SettingsPageProps) {
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

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
          <div className="setting-fact"><span>浏览器访问</span><strong>http://127.0.0.1:9812</strong></div>
          <div className="setting-fact"><span>监听范围</span><strong className="safe-text">仅限本机</strong></div>
          <p>点击桌面窗口右上角关闭按钮时会同步停止本机服务；浏览器访问不提供停服操作。</p>
        </section>

        <section className="settings-section compact-setting about-setting">
          <div className="compact-heading"><Building2 size={18} /><h2>关于</h2></div>
          <div className="setting-fact"><span>产品</span><strong>企业微信记录归档</strong></div>
          <div className="setting-fact"><span>版本</span><strong>0.0.4</strong></div>
        </section>
      </div>
    </main>
  );
}
