import {
  Download,
  RefreshCw,
  RotateCcw,
  Save,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  generateEnterpriseCollector,
  getEnterpriseConfig,
  openEnterpriseCollectorDirectory,
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
}

export function ServerConfigPage({ token, onTokenChanged }: ServerConfigPageProps) {
  const [organizationName, setOrganizationName] = useState(DEFAULT_ORGANIZATION_NAME);
  const [collectionNotice, setCollectionNotice] = useState(DEFAULT_COLLECTION_NOTICE);
  const [uploadUrl, setUploadUrl] = useState("http://127.0.0.1:8787");
  const [keyId, setKeyId] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  const [tokenDraft, setTokenDraft] = useState(token);
  const [includeMedia, setIncludeMedia] = useState(false);
  const [dataRedaction, setDataRedaction] = useState(false);
  const [offlineExportEnabled, setOfflineExportEnabled] = useState(false);
  const [status, setStatus] = useState<string>();
  const [statusTone, setStatusTone] = useState<"success" | "error">("success");
  const [busy, setBusy] = useState(false);
  const [collectorDirectoryReady, setCollectorDirectoryReady] = useState(false);

  useEffect(() => {
    getEnterpriseConfig(token)
      .then((config) => {
        setOrganizationName(config.organizationName || DEFAULT_ORGANIZATION_NAME);
        setCollectionNotice(config.collectionNotice);
        setUploadUrl(config.uploadUrl || "http://127.0.0.1:8787");
        setKeyId(config.keyId);
        setKeyDraft(config.keyId);
        setIncludeMedia(config.includeMedia ?? false);
        setDataRedaction(config.dataRedaction ?? false);
        setOfflineExportEnabled(config.offlineExportEnabled ?? false);
      })
      .catch(() => showStatus("无法读取采集端配置。", "error"));
  }, [token]);

  useEffect(() => {
    setTokenDraft(token);
  }, [token]);

  const showStatus = (message: string, tone: "success" | "error" = "success") => {
    setStatus(message);
    setStatusTone(tone);
  };

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setStatus(undefined);
    setCollectorDirectoryReady(false);
    try {
      await action();
    } catch (error) {
      showStatus(error instanceof Error ? error.message : "操作失败，请稍后重试。", "error");
    } finally {
      setBusy(false);
    }
  };

  const saveEnterpriseKey = () => run(async () => {
    const nextKeyId = keyDraft.trim();
    if (nextKeyId.length < 24) return;
    const config = await updateEnterpriseConfig(token, {
      organizationName: organizationName || DEFAULT_ORGANIZATION_NAME,
      collectionNotice,
      uploadUrl,
      keyId: nextKeyId,
      includeMedia,
      dataRedaction,
      offlineExportEnabled,
    });
    setKeyId(config.keyId);
    setKeyDraft(config.keyId);
    showStatus("加密密钥已更新；新生成的采集端会使用该密钥。");
  });

  const generateCollector = () => run(async () => {
    const config = await updateEnterpriseConfig(token, {
      organizationName: organizationName || DEFAULT_ORGANIZATION_NAME,
      collectionNotice,
      uploadUrl,
      keyId: keyId || undefined,
      includeMedia,
      dataRedaction,
      offlineExportEnabled,
    });
    setKeyId(config.keyId);
    setKeyDraft(config.keyId);
    const result = await generateEnterpriseCollector(token);
    setCollectorDirectoryReady(result.executableGenerated);
    showStatus(result.executableGenerated
      ? `采集端已生成：${result.fileName}`
      : "密钥已保存，但未能生成采集端文件。");
  });

  const openCollectorDirectory = () => run(async () => {
    await openEnterpriseCollectorDirectory(token);
    showStatus("已打开采集端目录。");
  });

  const saveAccessToken = () => run(async () => {
    const nextToken = tokenDraft.trim();
    if (nextToken.length < 24) return;
    await updateServerAccessToken(token, nextToken);
    onTokenChanged(nextToken);
    setTokenDraft(nextToken);
    showStatus("访问令牌已更新并立即生效。");
  });

  const regenerateAccessToken = () => run(async () => {
    const result = await regenerateServerAccessToken(token);
    onTokenChanged(result.accessToken);
    setTokenDraft(result.accessToken);
    showStatus("已随机生成新的访问令牌，其他浏览器会话需要使用新令牌重新连接。");
  });

  const regenerateKey = () => run(async () => {
    const config = await rotateEnterpriseKey(token);
    setKeyId(config.keyId);
    setKeyDraft(config.keyId);
    showStatus("加密密钥已重新生成；只有生成过采集端的旧密钥会保留用于解密历史数据。");
  });

  const keyChanged = keyDraft.trim() !== keyId;
  const tokenChanged = tokenDraft.trim() !== token;

  return (
    <main className="settings-page collector-settings-page">
      {status && <Toast tone={statusTone} duration={statusTone === "error" ? 6000 : 6000} actionLabel={collectorDirectoryReady ? "打开目录" : undefined} onAction={collectorDirectoryReady ? () => void openCollectorDirectory() : undefined} onClose={() => { setStatus(undefined); setCollectorDirectoryReady(false); }}>{status}</Toast>}

      <div className="server-config-layout collector-layout">
        <section className="settings-section collection-settings collector-settings-card">
          <div className="collector-top-config">
            <div className="collector-config-field">
              <div className="collector-config-label"><label htmlFor="upload-url" title="采集端上传加密数据的服务端地址，支持 http 或 https。">服务端 URL：</label></div>
              <div className="config-value-row url-config-row">
                <input id="upload-url" value={uploadUrl} onChange={(event) => setUploadUrl(event.target.value)} placeholder="https://archive.example.com" autoComplete="url" />
                <label className="offline-export-option" title="采集端可将加密数据导出为 .wca 文件，供管理员离线导入。"><input type="checkbox" checked={offlineExportEnabled} onChange={(event) => setOfflineExportEnabled(event.target.checked)} />支持离线导出</label>
                <button className="config-icon-button primary config-save-button" type="button" aria-label="保存服务端 URL" title="保存服务端 URL" onClick={() => void run(async () => { await updateEnterpriseConfig(token, { organizationName: organizationName || DEFAULT_ORGANIZATION_NAME, collectionNotice, uploadUrl, keyId: keyId || undefined, includeMedia, dataRedaction, offlineExportEnabled }); showStatus("服务端 URL 已保存。" ); })} disabled={busy || !uploadUrl.trim()}><Save size={16} /></button>
              </div>
            </div>
            <div className="collector-config-field">
              <div className="collector-config-label"><label htmlFor="access-token" title="用于验证浏览器和本机 API 请求，不参与数据加解密。">访问令牌：</label></div>
              <div className="config-value-row">
                <input id="access-token" className="code-input" value={tokenDraft} onChange={(event) => setTokenDraft(event.target.value)} placeholder="至少 24 个字符" autoComplete="off" />
                {tokenChanged && <button className="config-icon-button" type="button" aria-label="还原服务端访问令牌" title="还原当前令牌" onClick={() => setTokenDraft(token)} disabled={busy}><RotateCcw size={16} /></button>}
                <button className="config-icon-button config-refresh-button" type="button" aria-label="重新生成服务端访问令牌" title="随机重新生成" onClick={regenerateAccessToken} disabled={busy}><RefreshCw size={16} /></button>
                <button className="config-icon-button primary config-save-button" type="button" aria-label="保存服务端访问令牌" title="保存令牌" onClick={saveAccessToken} disabled={busy || !tokenChanged || tokenDraft.trim().length < 24}><Save size={16} /></button>
              </div>
            </div>
          </div>

          <div className="collector-security-panel">
            <div className="collector-security-grid">
              <div className="collector-config-field">
                <div className="collector-config-label"><label htmlFor="enterprise-key" title="用于采集端加密和服务端解密，实际私钥由服务端 DPAPI 保护。">加密密钥：</label></div>
                <div className="config-value-row">
                  <input id="enterprise-key" className="code-input" value={keyDraft} onChange={(event) => setKeyDraft(event.target.value)} placeholder="至少 24 个字符" autoComplete="off" />
                  {keyChanged && <button className="config-icon-button" type="button" aria-label="还原加密密钥" title="还原当前密钥" onClick={() => setKeyDraft(keyId)} disabled={busy}><RotateCcw size={16} /></button>}
                  <button className="config-icon-button config-refresh-button" type="button" aria-label="重新生成加密密钥" title="重新生成密钥" onClick={regenerateKey} disabled={busy || !keyId}><RefreshCw size={16} /></button>
                  <button className="config-icon-button primary config-save-button" type="button" aria-label="保存加密密钥" title="保存密钥" onClick={saveEnterpriseKey} disabled={busy || !keyChanged || keyDraft.trim().length < 24}><Save size={16} /></button>
                </div>
              </div>
            </div>
          </div>

          <div className="collector-notice-panel">
            <div className="collector-section-heading">
              <h3><label htmlFor="collection-notice" title="采集开始前会展示给员工，用于说明采集范围和用途。">员工告知内容</label></h3>
            </div>
            <div className="settings-field">
              <textarea id="collection-notice" value={collectionNotice} onChange={(event) => setCollectionNotice(event.target.value)} rows={8} />
            </div>
          </div>

          <div className="collector-media-options">
            <label className="collector-media-label"><input type="checkbox" checked={includeMedia} onChange={(event) => setIncludeMedia(event.target.checked)} />含文件和图片</label>
            <label className="collector-media-label" title="上传前脱敏账号、密码、令牌、联系方式及疑似凭据片段；文件名和图片名保持原样。"><input type="checkbox" checked={dataRedaction} onChange={(event) => setDataRedaction(event.target.checked)} />数据脱敏</label>
          </div>

          <footer className="collector-generate-row">
            <button className="primary-button collector-generate-button" type="button" onClick={generateCollector} disabled={busy || !collectionNotice.trim()}><Download size={16} />生成采集端</button>
          </footer>
        </section>
      </div>
    </main>
  );
}
