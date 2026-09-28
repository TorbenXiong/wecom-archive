# 企业微信记录归档

独立实现的 Windows 企业微信归档工具。发布包包含 `WeComArchive.exe` 工作台：工作台负责归档、管理节点和计划，并按服务端配置生成已绑定的采集端。所有源数据库均通过只读快照解析，不修改企业微信数据。

## 功能

- 浏览、全局搜索和导出会话，展示成员名称、引用消息、群公告、图片和文件；搜索结果可直接定位到原消息。
- 立即采集本机文本或完整内容，也可导入专属采集端生成的 `.wca` 文件。
- 在统一“采集计划”页面维护服务端多条本机计划和远程采集端计划，查看节点在线状态、最近执行和配置同步状态。
- 配置服务端、凭据、采集范围和脱敏策略后生成已绑定采集端；采集端无需用户填写服务端地址、注册码或密钥，采集结果按源消息 ID 增量、幂等合并。
- 导出时可分别设置会话和会话内容的升降序；界面在全量消息排序后分页。
- 导出 JSON、CSV、HTML 或 Markdown 单文件，默认启用数据脱敏、简化信息和美化信息；阅读型 JSON、HTML 和 Markdown 按会话分组，CSV 按会话连续输出；关闭美化时输出紧凑格式。
- 简化导出保留会话、发送人、时间、消息类型、正文和附件名称，移除系统 ID、原始数据和群目录。简化 JSON 仅供阅读，完整 JSON 可重新导入。
- 媒体按内容哈希流式采集，图片可预览，文件通过 Windows 默认应用打开；不生成 ZIP 或校验旁车文件。

`WeComArchive.exe` 在同一进程内运行桌面窗口、本机归档服务和服务端生成的采集端运行时，默认监听 `127.0.0.1:9812`。窗口可隐藏并按计划后台运行；退出程序会停止服务和任务。生成的采集端使用服务端预置的绑定身份和签名配置，并按心跳同步后续修改。

当已保存的采集端服务端 URL 使用明确的私有局域网 IP（例如 `http://10.10.1.202:9812`）时，服务端启动会自动监听该地址，已生成采集端无需另外配置监听参数。环境变量 `WECOM_ARCHIVE_SERVER_LISTEN` 仍可作为显式覆盖；域名、公网地址或无效 URL 不会触发自动局域网监听。Windows 防火墙仍需允许对应 TCP 端口。

启用计划的专属采集端首次采集上传后收起到系统托盘；关闭采集端窗口也会收起，右键托盘图标选择“退出采集端”才会停止运行。托盘提示展示上传状态和计划时间。

## 工程结构

```text
apps/server/          归档 API 与桌面工作台服务
src/                  React 工作台与采集端界面
src-tauri/            Windows 桌面宿主和白名单 commands
crates/domain/        版本化归档契约
crates/source-windows 数据发现、快照、探针和 DPAPI
crates/parser/        消息归一化
crates/transfer/      采集传输与数据脱敏
crates/archive-store/ SQLite/FTS5 归档、修订和审计
crates/export/        JSON/CSV/HTML/Markdown 导出
```

详细设计见 [`docs/architecture.md`](docs/architecture.md)，安全边界见 [`docs/privacy-security.md`](docs/privacy-security.md)，当前能力见 [`docs/capability-matrix.md`](docs/capability-matrix.md)，发布前状态见 [`docs/implementation-status.md`](docs/implementation-status.md)。

## 开发

工具链：Rust `1.98.1`、Node.js `>=24`、pnpm `10.30.3`。版本与来源见 [`docs/dependency-plan.md`](docs/dependency-plan.md)。

```powershell
pnpm test
pnpm exec tsc -b
pnpm build:client
pnpm build:server
cargo test --workspace --locked
cargo build --release --locked --bin WeComArchive
```

`build:client` 生成采集端界面资源，`build:server` 生成归档工作台资源；采集端文件由服务端配置页按需生成。

## 运行数据

- 工作台使用 exe 同级 `serverData/` 保存访问令牌、加密私钥、归档库、媒体、采集端和导出文件。
- 本机采集和专属采集端均使用系统临时目录处理快照，完成后清理。
- 服务端生成的采集端在 exe 尾部携带签名绑定配置，并在 exe 同级以 DPAPI 保护最近一次有效配置；服务端配置在下次任务前生效。

## 安全边界

工作台默认仅监听本机。已生成采集端仅在签名绑定配置启用后台计划后连接服务端；外网地址应使用 HTTPS。程序不包含遥测、更新检查、远程字体或第三方资源。日志仅记录阶段、计数、哈希前缀和稳定错误码，不记录聊天正文、凭据、密钥或私人路径。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
