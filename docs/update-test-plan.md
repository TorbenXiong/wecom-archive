# 升级功能测试方案

## 已自动验证

以下命令在 Windows x64、Rust `1.98.1`、Node.js/pnpm 本地缓存环境执行，不下载依赖：

```powershell
pnpm exec tsc -b
pnpm test -- --maxWorkers 1
cargo test --workspace --locked --offline
pnpm build:client
pnpm build:server
cargo build --release --locked --offline --bin WeComArchive
```

本次结果：Rust 工作区测试全部通过（真实企业微信授权测试按设计忽略），前端 11 个测试文件、67 项通过，TypeScript、前端构建和 Release 构建通过。

单文件启动验收使用全新临时目录复制 `WeComArchive-0.0.9.exe`：

1. 设置临时监听端口并启动 EXE。
2. 请求 `/api/v1/health`，确认返回 `status=ok` 和 `version=0.0.9`。
3. 请求 `/` 和其中的 JavaScript 资源，确认界面资源已嵌入 EXE。
4. 检查 EXE 同级目录，确认首次启动创建 `serverData/`，没有创建 `collectorData/`。
5. 关闭进程，确认本机服务正常退出。

## 发布前手工验收

### 工作台升级

准备当前版本 EXE 和下一个版本 EXE，分别放在两个独立目录。Release 需要包含：

- `WeComArchive-x.y.z.exe`
- `WeComArchive-x.y.z.exe.sha256`，内容为 64 位 SHA-256 和文件名
- GitHub Release API，或 GitLab Release API/asset links

在旧版本工作台的“设置 → 关于”中点击“检查更新”：

1. 确认能显示新版本号。
2. 点击升级，确认工作台退出并重新启动。
3. 确认 `serverData/`、归档数据库、访问令牌和服务端配置仍存在。
4. 重复检查更新，确认当前版本已是最新版本。
5. 用错误 SHA-256 发布测试附件，确认升级失败且旧 EXE 仍可启动。
6. 关闭程序后锁定 EXE 或模拟替换失败，确认旧文件未丢失。

### 采集端升级

先用当前工作台生成已绑定采集端，在采集端管理中确认版本和在线心跳。完成工作台升级后：

1. 确认采集端管理显示“可升级”。
2. 点击“升级”，确认请求通过已认证服务端下发。
3. 等采集端空闲，确认它下载当前服务端 EXE 并自动重启。
4. 确认采集端身份、组织、密钥、计划、日志设置和 DPAPI 配置仍有效。
5. 确认升级后的采集端继续心跳和采集上传。
6. 使用篡改后的签名、错误 SHA-256 或错误 `collectorId`，确认采集端拒绝替换并保留旧版本。

### 网络与安全

1. 使用 HTTP 更新地址，确认客户端拒绝请求。
2. 使用带用户名或密码的 URL，确认客户端拒绝请求。
3. 使用草稿、预发布或非 `x.y.z` Release，确认客户端不显示可升级版本。
4. 使用不匹配的 EXE 文件名、非 Windows x64 PE 文件或超过大小限制的文件，确认客户端拒绝下载或替换。
5. 断开网络后确认工作台和采集端原有本地归档、计划和离线导出不受影响。

## 发布产物

升级目标 Release 单文件：

- `artifacts/0.0.10/WeComArchive-0.0.10.exe`
- `artifacts/0.0.10/WeComArchive-0.0.10.exe.sha256`

升级目标 EXE SHA-256：`4d568ea6000955967734b3292bb4cc942fa7221d2c4561395cfc7fa80d6524de`

该 EXE 未进行 Authenticode 签名，发布到 GitHub/GitLab 前应按组织发布流程签名或补充可信发布渠道。
