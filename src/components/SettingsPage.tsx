import { Building2, Database, Server } from "lucide-react";

export function SettingsPage() {
  return (
    <main className="settings-page">
      <div className="general-settings-grid">
        <section className="settings-section compact-setting">
          <div className="compact-heading"><Database size={18} /><h2>归档存储</h2></div>
          <div className="setting-fact"><span>存储方式</span><strong>本机单节点归档库</strong></div>
          <div className="setting-fact"><span>数据目录</span><strong>serverData</strong></div>
          <p>支持全文检索、幂等导入、消息修订和完整性校验。</p>
        </section>

        <section className="settings-section compact-setting">
          <div className="compact-heading"><Server size={18} /><h2>网络与运行</h2></div>
          <div className="setting-fact"><span>浏览器访问</span><strong>http://127.0.0.1:8787</strong></div>
          <div className="setting-fact"><span>监听范围</span><strong className="safe-text">仅限本机</strong></div>
          <p>点击桌面窗口右上角关闭按钮时会同步停止本机服务；浏览器访问不提供停服操作。</p>
        </section>

        <section className="settings-section compact-setting about-setting">
          <div className="compact-heading"><Building2 size={18} /><h2>关于</h2></div>
          <div className="setting-fact"><span>产品</span><strong>企微归档 · 企业版</strong></div>
          <div className="setting-fact"><span>版本</span><strong>0.1.0</strong></div>
        </section>
      </div>
    </main>
  );
}
