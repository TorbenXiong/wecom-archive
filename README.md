# 企业微信记录归档

独立实现的 Windows 企业微信归档工具。发布包包含 `WeComArchive.exe` 工作台：工作台负责归档、管理节点和计划，并按服务端配置生成已绑定的采集端。所有源数据库均通过只读快照解析，不修改企业微信数据。

## 功能

- 浏览、全局搜索和导出会话，展示成员名称、引用消息、群公告、图片和文件；搜索结果可直接定位到原消息。
- 立即采集本机文本或完整内容，也可导入专属采集端生成的 `.wca` 文件。
- 自动读取当前 Windows 用户的企业微信存储目录设置；找不到数据时，可在工作台采集或快速导出中填写本机目录，采集成功后加密记住。
- 在“采集端管理”中维护本机和远程采集计划，查看在线状态、最近成功、下次执行和配置同步状态。
- 配置服务端、凭据、采集范围和脱敏策略后生成已绑定采集端；采集端无需用户填写服务端地址、注册码或密钥，采集结果按源消息 ID 增量、幂等合并。
- 导出时可分别设置会话和会话内容的升降序；界面在全量消息排序后分页。
- 导出 JSON、CSV、HTML 或 Markdown 单文件，默认启用数据脱敏、简化信息和美化信息；阅读型 JSON、HTML 和 Markdown 按会话分组，CSV 按会话连续输出；关闭美化时输出紧凑格式。
- 简化导出保留会话、发送人、时间、消息类型、正文和附件名称，移除系统 ID、原始数据和群目录。简化 JSON 仅供阅读，完整 JSON 可重新导入。
- 媒体按内容哈希流式采集，图片可预览，文件通过 Windows 默认应用打开；不生成 ZIP 或校验旁车文件。

`WeComArchive.exe` 在同一进程内运行桌面窗口、本机归档服务和服务端生成的采集端运行时，默认监听 `127.0.0.1:9812`。窗口可隐藏并按计划后台运行；退出程序会停止服务和任务。生成的采集端使用服务端预置的绑定身份和签名配置，并按心跳同步后续修改。

当已保存的采集端服务端 URL 使用明确的私有局域网 IP（例如 `http://10.10.1.202:9812`）时，服务端启动会自动监听该地址，已生成采集端无需另外配置监听参数。环境变量 `WECOM_ARCHIVE_SERVER_LISTEN` 仍可作为显式覆盖；域名、公网地址或无效 URL 不会触发自动局域网监听。Windows 防火墙仍需允许对应 TCP 端口。

## 采集计划

- 支持固定间隔和每日定时。采集端启动时已启用计划，或运行中从未启用改为启用，均会先采集并上传一次，再按计划执行；远程修改通过心跳同步后生效。
- 远程“最近成功”只记录上传成功时间；准备数据、保存配置、失败尝试和无变化跳过上传均不更新。本机计划记录成功采集时间。
- 采集与上传共用任务锁；启用计划后的首次任务遇到忙碌会等待，不跳过。首次上传允许空结果，普通周期无消息变化时跳过上传。
- 关闭采集端窗口会收起到托盘；右键选择“退出采集端”停止运行。窗口和托盘分别展示状态、最近成功和下次执行。

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

详细设计见 [`docs/architecture.md`](docs/architecture.md)，安全边界见 [`docs/privacy-security.md`](docs/privacy-security.md)，当前能力见 [`docs/capability-matrix.md`](docs/capability-matrix.md)，验证状态见 [`docs/implementation-status.md`](docs/implementation-status.md)。

## 开发

工具链：Rust `1.98.1`、Node.js `>=24`、pnpm `10.30.3`。版本与来源见 [`docs/dependency-plan.md`](docs/dependency-plan.md)。

```powershell
pnpm test
pnpm exec tsc -b
pnpm build:client
pnpm build:server
cargo test --workspace --locked --offline
cargo build --release --locked --offline --bin WeComArchive
```

`build:client` 生成采集端界面资源，`build:server` 生成归档工作台资源；采集端文件由服务端配置页按需生成。
前端构建完成后再编译 Rust，避免嵌入旧资源。以上命令要求工具链和依赖已在本机准备好；`--offline` 不下载缺失依赖。

## 运行数据

- 自动发现合并企业微信注册表配置、记住的目录及原有默认目录；优先当前账号数据，历史 `Backup` 数据作为后备候选。工作台、采集端发现和后台任务复用记忆目录。
- 手动填写目录重试时只扫描该目录，避免默认目录中的其他账号被误选。目录须是本机磁盘上的子目录；不接受整盘根目录、网络路径或父目录跳转。目录配置缺失、损坏或失效时，自动发现仍可回退到默认目录。
- 手动确认过的目录以当前 Windows 用户的 DPAPI 保护，保存在 exe 同级 `source-roots.dpapi`，最多保存 3 条；移动程序时需一并保留此文件，换 Windows 用户或电脑后需重新填写。
- 工作台使用 exe 同级 `serverData/` 保存配置、归档、媒体、采集端和导出文件；采集端使用 exe 同级 `collectorData/` 保存运行配置和离线导出。
- 本机和采集端在 `collectorData/work/<run-id>/` 处理只读快照，任务完成或失败后清理。采集端尾部携带签名绑定配置，最新有效配置以 DPAPI 保护保存在 `collectorData/instances/<collectorId>/`。

日志默认关闭，可在“生成默认配置”和“采集端管理”中启用采集端日志；环境变量 `WECOM_ARCHIVE_COLLECTOR_LOG`、`WECOM_ARCHIVE_SERVER_LOG` 可设置启动默认值。日志按日期分文件，时间格式为 `yyyy-MM-dd HH:mm:ss.SSS`。

| 日志 | exe 同级目录 |
| --- | --- |
| 采集端本地日志 | `collectorData/logs/YYYY-MM-DD.log` |
| 服务端自身日志 | `serverData/logs/server/YYYY-MM-DD.log` |
| 服务端接收采集端日志 | `serverData/logs/collector/<collectorId>/YYYY-MM-DD.log` |

管理页展示实例对应的服务端日志目录；名称或 IP 变更不影响关联。

## 安全边界

工作台默认仅监听本机。已生成采集端按签名绑定配置向已认证服务端同步心跳；自动上传须启用采集计划，也可由用户手动上传。外网地址应使用 HTTPS。程序不包含遥测、更新检查、远程字体或第三方资源；日志不记录聊天正文、凭据、密钥或私人路径。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
