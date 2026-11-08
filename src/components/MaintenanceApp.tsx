import { useEffect, useState } from "react";
import { Database, LoaderCircle } from "lucide-react";
import { activateRestored, createBackup, maintenanceStatus, restoreBackup, verifyBackup, type MaintenanceResult } from "../lib/maintenance";
import "../styles.css";

export function MaintenanceApp() {
  const backupLocation = Boolean(window.__WECOM_BACKUP_LOCATION__);
  const [pending, setPending] = useState<boolean>();
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [message, setMessage] = useState("");
  const [result, setResult] = useState<MaintenanceResult>();
  useEffect(() => {
    let active = true;
    void maintenanceStatus().then((value) => { if (active) setPending(value); })
      .catch(() => { if (active) setError("无法读取恢复状态，请关闭窗口后重试。"); });
    return () => { active = false; };
  }, []);

  const run = async (label: string, operation: () => Promise<MaintenanceResult | null>) => {
    if (busy) return;
    setBusy(label); setError(""); setMessage(""); setResult(undefined);
    try {
      const next = await operation();
      if (next) { setResult(next); setMessage(`${label}完成。`); }
      else setMessage("已取消目录选择，未执行操作。");
    } catch (reason) {
      setError(typeof reason === "string" ? reason : "操作未完成，原数据已保留。请重试。");
    } finally { setBusy(""); }
  };

  const activate = async () => {
    if (busy) return;
    setBusy("确认恢复"); setError(""); setMessage("");
    try { await activateRestored(); setMessage("正在打开恢复后的工作台…"); }
    catch (reason) { setError(typeof reason === "string" ? reason : "确认恢复失败，请重试。"); setBusy(""); }
  };

  return <main className="maintenance-page">
    <div className="compact-heading"><Database size={24} /><h1>备份与恢复</h1></div>
    <p>此窗口不启动归档服务和采集计划。操作前请退出使用同一数据目录的工作台。</p>
    {backupLocation && <p role="note">当前程序位于备份目录。请校验或恢复到新目录后使用，备份本身不会启动归档服务或采集计划。</p>}
    {pending && !backupLocation && <section className="settings-section">
      <h2>恢复待确认</h2>
      <p>本次恢复尚未启动服务。确认后，本机采集计划会暂停，原采集端会停用，服务端地址会重置为本机。请进入工作台重新核对设备授权、计划和服务监听地址，再按需启用。</p>
      <button className="primary-button" disabled={Boolean(busy)} onClick={() => void activate()}>确认恢复并打开工作台</button>
    </section>}
    {!backupLocation && <section className="settings-section">
      <h2>创建完整备份</h2>
      <p>选择备份存放位置。程序会新建目录，保存当前程序、归档库、附件、服务端配置和受保护的凭据，并逐文件校验。临时采集文件和快照缓存不纳入备份。</p>
      <button className="secondary-button" disabled={Boolean(busy) || pending === undefined} onClick={() => void run("创建备份", createBackup)}>选择位置并创建备份</button>
    </section>}
    <section className="settings-section">
      <h2>校验与恢复</h2>
      <p>先选择备份目录。恢复时再选择存放位置，程序会创建独立目录并保留原数据。完成后打开新目录中的 WeComArchive.exe，确认恢复后使用。</p>
      <p>备份含敏感数据，请妥善保管。当前仅支持备份所属电脑及 Windows 用户，使用备份对应版本恢复；完整 JSON 导出不能替代备份。</p>
      <div className="settings-update-actions">
        <button className="secondary-button" disabled={Boolean(busy) || pending === undefined} onClick={() => void run("校验备份", verifyBackup)}>选择备份并校验</button>
        <button className="primary-button" disabled={Boolean(busy) || pending === undefined} onClick={() => void run("恢复到新目录", restoreBackup)}>选择备份并恢复</button>
      </div>
    </section>
    {busy && <p role="status" className="settings-inline-status"><LoaderCircle className="spin" size={16} />正在{busy}，复制和校验可能需要较长时间，请保持窗口打开…</p>}
    {message && <p role="status">{message}</p>}
    {result && <div className="maintenance-result"><p>目录：{result.directory}</p><p>{result.fileCount} 个文件 · {(result.bytes / 1024 / 1024).toFixed(2)} MiB</p></div>}
    {error && <p role="alert" className="settings-error">{error}</p>}
  </main>;
}
