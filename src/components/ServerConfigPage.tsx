import {
  CheckCircle2,
  Clipboard,
  Download,
  KeyRound,
  RefreshCw,
  ShieldCheck,
  Upload,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  generateEnterpriseCollector,
  getEnterpriseConfig,
  importEnterprisePackage,
  regenerateServerAccessToken,
  rotateEnterpriseKey,
  updateEnterpriseConfig,
  updateServerAccessToken,
} from "../lib/server-api";
import { Toast } from "./Overlays";

const DEFAULT_ORGANIZATION_NAME = "本企业";
const DEFAULT_COLLECTION_NOTICE = "加密传输本机企业微信聊天记录到服务端";

interface ServerConfigPageProps {
  token: string;
  onTokenChanged: (token: string) => void;
  onImported: () => Promise<void>;
}

export function ServerConfigPage({ token, onTokenChanged, onImported }: ServerConfigPageProps) {
  const [organizationName, setOrganizationName] = useState(DEFAULT_ORGANIZATION_NAME);
  const [collectionNotice, setCollectionNotice] = useState(DEFAULT_COLLECTION_NOTICE);
  const [keyId, setKeyId] = useState("");
  const [newAccessToken, setNewAccessToken] = useState("");
  const [status, setStatus] = useState<string>();
  const [statusTone, setStatusTone] = useState<"success" | "error">("success");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    getEnterpriseConfig(token)
      .then((config) => {
        setOrganizationName(config.organizationName || DEFAULT_ORGANIZATION_NAME);
        setCollectionNotice(config.collectionNotice);
        setKeyId(config.keyId);
      })
      .catch(() => showStatus("无法读取服务端配置。", "error"));
  }, [token]);

  const showStatus = (message: string, tone: "success" | "error" = "success") => {
    setStatus(message);
    setStatusTone(tone);
  };

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setStatus(undefined);
    try {
      await action();
    } catch (error) {
      showStatus(error instanceof Error ? error.message : "操作失败，请稍后重试。", "error");
    } finally {
      setBusy(false);
    }
  };

  const saveCollectionConfig = () => run(async () => {
    const config = await updateEnterpriseConfig(token, {
      organizationName: organizationName || DEFAULT_ORGANIZATION_NAME,
      collectionNotice,
      keyId: keyId || undefined,
    });
    setKeyId(config.keyId);
    showStatus("采集端配置已保存。");
  });

  const generateCollector = () => run(async () => {
    const config = await updateEnterpriseConfig(token, {
      organizationName: organizationName || DEFAULT_ORGANIZATION_NAME,
      collectionNotice,
      keyId: keyId || undefined,
    });
    setKeyId(config.keyId);
    const result = await generateEnterpriseCollector(token);
    showStatus(result.executableGenerated
      ? `采集端已生成：${result.fileName}`
      : "密钥已保存，但未能生成采集端文件。");
  });

  const importPackage = (file?: File) => {
    if (!file) return;
    void run(async () => {
      const result = await importEnterprisePackage(file, token);
      await onImported();
      showStatus(`加密包已解密并导入，新增 ${result.inserted.toLocaleString("zh-CN")} 条消息。`);
    });
  };

  const copyAccessToken = () => run(async () => {
    await navigator.clipboard.writeText(token);
    showStatus("当前访问令牌已复制到剪贴板。");
  });

  const saveAccessToken = () => run(async () => {
    const nextToken = newAccessToken.trim();
    if (nextToken.length < 24) return;
    await updateServerAccessToken(token, nextToken);
    onTokenChanged(nextToken);
    setNewAccessToken("");
    showStatus("访问令牌已更新并立即生效。");
  });

  const regenerateAccessToken = () => run(async () => {
    const result = await regenerateServerAccessToken(token);
    onTokenChanged(result.accessToken);
    setNewAccessToken("");
    showStatus("已随机生成新的访问令牌，其他浏览器会话需要使用新令牌重新连接。");
  });

  const regenerateKey = () => run(async () => {
    const config = await rotateEnterpriseKey(token);
    setKeyId(config.keyId);
    showStatus("企业加密密钥已重新生成；只有生成过采集端的旧密钥会保留用于解密历史数据。");
  });

  return (
    <main className="settings-page">
      {status && <Toast tone={statusTone} duration={statusTone === "error" ? 6000 : 4000} onClose={() => setStatus(undefined)}>{status}</Toast>}

      <div className="server-config-layout">
        <section className="settings-section collection-settings">
          <div className="compact-heading"><ShieldCheck size={18} /><h2>采集端配置</h2></div>
          <div className="settings-form">
            <div className="settings-field">
              <label htmlFor="collection-notice">员工告知内容</label>
              <textarea id="collection-notice" value={collectionNotice} onChange={(event) => setCollectionNotice(event.target.value)} rows={5} />
            </div>
          </div>
          <div className="settings-actions">
            <button className="primary-button" type="button" onClick={saveCollectionConfig} disabled={busy || !collectionNotice.trim()}><CheckCircle2 size={16} />保存配置</button>
            <button className="secondary-button" type="button" onClick={generateCollector} disabled={busy || !collectionNotice.trim()}><Download size={16} />生成采集端</button>
          </div>
          <div className="import-setting">
            <div><h3>导入加密包</h3><p>选择采集端生成的 `.wca` 文件，服务端会匹配对应的历史密钥并解密入库。</p></div>
            <label className="secondary-button file-action"><Upload size={16} />选择并导入<input type="file" accept=".wca,application/octet-stream" hidden disabled={busy} onChange={(event) => { importPackage(event.target.files?.[0]); event.currentTarget.value = ""; }} /></label>
          </div>
        </section>

        <aside className="settings-secondary">
          <section className="settings-section compact-setting">
            <div className="compact-heading"><KeyRound size={18} /><h2>企业加密密钥</h2></div>
            <div className="setting-fact"><span>当前版本</span><strong className="code-value">{keyId || "正在读取…"}</strong></div>
            <p>重新生成后，新采集端使用新密钥；历史密钥继续用于解密已有采集端的数据。</p>
            <button className="secondary-button compact-full" type="button" onClick={regenerateKey} disabled={busy || !keyId}><RefreshCw size={15} />重新生成密钥</button>
          </section>

          <section className="settings-section compact-setting token-setting">
            <div className="compact-heading"><KeyRound size={18} /><h2>服务端访问令牌</h2></div>
            <p>用于验证浏览器和本机 API 请求，不参与数据加解密。当前值可以直接复制。</p>
            <div className="settings-field">
              <label htmlFor="current-access-token">当前令牌</label>
              <div className="token-value-row">
                <input id="current-access-token" className="code-input" value={token} readOnly />
                <button className="icon-text-button" type="button" onClick={copyAccessToken} disabled={busy}><Clipboard size={15} />复制</button>
              </div>
            </div>
            <div className="settings-field token-manual-field">
              <label htmlFor="new-access-token">自定义新令牌</label>
              <input id="new-access-token" type="text" autoComplete="off" value={newAccessToken} onChange={(event) => setNewAccessToken(event.target.value)} placeholder="至少 24 个字符" />
            </div>
            <div className="token-actions">
              <button className="secondary-button" type="button" onClick={saveAccessToken} disabled={busy || newAccessToken.trim().length < 24}>保存新令牌</button>
              <button className="secondary-button" type="button" onClick={regenerateAccessToken} disabled={busy}><RefreshCw size={15} />随机重新生成</button>
            </div>
          </section>
        </aside>
      </div>
    </main>
  );
}
