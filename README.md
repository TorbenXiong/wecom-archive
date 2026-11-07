# 企业微信记录归档

独立实现的 Windows 企业微信归档工具。发布包包含 `WeComArchive.exe` 工作台：工作台负责归档、管理节点和计划，并按服务端配置生成已绑定的采集端。所有源数据库均通过只读快照解析，不修改企业微信数据。

## 功能

- 按需加载、全局搜索和导出会话，展示成员名称、引用消息、群公告、图片和文件；搜索结果与引用消息均可跨页定位到原消息。
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
- 采集与上传共用任务锁；启用计划后的首次任务遇到忙碌会等待，不跳过。首次上传允许空结果，普通周期无消息、目录或成员资料变化时跳过上传。
- 远程计划首次全量读取，随后从上次成功扫描的开始时间前移 5 分钟读取，仅上传新增或变化的消息及相关附件；进度持久化，失败不推进，重启后可继续。每隔 24 小时在下一次任务中全量核对旧消息修订和附件补齐。开启离线导出时保留全量本地读取，上传仍按变化筛选。媒体目录索引按需建立：无附件或本地路径可直接命中时跳过通用索引，语音只扫描 Voice 相关分支；媒体索引仅在本次采集中复用。可选的跨周期数据库快照低写入模式通过 `WECOM_ARCHIVE_SNAPSHOT_CACHE=1` 启用，默认关闭；每周期完整比对源文件，只写变化块，缓存不可用时回退完整复制。缓存可能含未脱敏的原始源内容，可停机删除，详见[采集说明](docs/local-collection-fixes.md)。媒体回退查找仍可能扫描全部目录，不代表采集过程所有阶段均已增量化。
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

本机采集、会话分类及人员资料的缓存限制见 [本机采集说明](docs/local-collection-fixes.md)；当前版本变化见 [0.0.16 版本说明](docs/release-0.0.16.md)，升级操作见 [升级功能测试方案](docs/update-test-plan.md)。语音支持读取企业微信已缓存的转写文字；“包含全内容”可按消息摘要采集本机 SILK 原文件。点击语音气泡可离线播放或暂停，并显示时长与进度；同一行的下载图标保存原始 `.silk` 文件。

合成数据性能基准见 [`docs/benchmarks.md`](docs/benchmarks.md)，使用 PowerShell 运行 `./scripts/benchmark-archive.ps1`。

会话消息相邻翻页与定位后的取页使用消息锚点，随机页码及失效锚点保留 OFFSET；兼容性、存储层对照和限制见 [深分页与消息定位](docs/message-pagination.md)。

消息图片进入滚动视区附近时按需读取，点击预览复用已读取内容；资源生命周期与验证范围见 [图片按需加载](docs/image-loading.md)。

大包内存分配对照见 [`docs/package-memory.md`](docs/package-memory.md)，覆盖 JSON 导入、加密包编码和密文字段解码；数据均为合成内容，默认离线，不使用真实密钥。

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

验收和打包使用 PowerShell，仅使用已有工具与依赖：

```powershell
./scripts/verify-desktop.ps1
./scripts/verify-real-environment.ps1 -Authorized
node scripts/acceptance/real-optimizations.mjs --authorized
./scripts/verify-real-recovery.ps1 -Authorized
./scripts/verify-storage-faults.ps1
./scripts/package-release.ps1 -IncludeRealEnvironment
```

桌面验收使用现有 `target/release/WeComArchive.exe` 和隔离合成数据；真实验收需要已登录的企业微信，默认检查源发现、探针、群资料和媒体采集，可用 `-Cases media` 单独重跑，单项默认等待 15 分钟。

`real-optimizations.mjs` 使用已有 Release 程序，在独立实例采集真实数据，验证三个本机计划周期、可选快照缓存复用、深页及定位对照、真实图片滚动加载与预览；仅在用户明确授权后运行，可能耗时较长。实例只监听本机，结束或失败均尝试清理本次真实测试数据并报告结果；不会更改已有工作台配置或源数据库。联合回归结果见 [优化验收记录](docs/acceptance/optimization-regression-2026-11-06.md)。

`verify-real-recovery.ps1 -Authorized` 验证真实数据完整备份、独立恢复及导入故障回滚，保留一份校验通过的本机备份并清理临时恢复和故障副本；仅在用户明确授权后执行。数据范围、保留位置及故障场景见 [真实恢复演练](docs/real-recovery-drill.md)。

`verify-storage-faults.ps1` 使用合成数据演练备份/恢复进程中断、导入提交前后中断和 SQLite 容量耗尽；真实 NTFS 写满使用需管理员运行的 `-BoundedVolume`，只操作新建的 256 MiB 虚拟磁盘。进程终止不代表物理断电，详见 [存储故障演练](docs/storage-fault-drill.md)。

本轮完整验证结果、真实数据规模及尚未覆盖的边界见 [备份恢复及导入验证总览](docs/acceptance/backup-import-validation-summary-2026-11-07.md)。

工作台导入、本机采集及归档导出支持请求响应丢失后的原任务续查；状态暂不可用时明确显示未知结果，避免重复执行。超时预算、记录保留和未覆盖边界见 [长任务状态与超时协调](docs/long-tasks.md)。

远程采集端上传支持独立的设备结果确认；结果未知时保留原标识，不推进增量进度，继续确认不会重复发送原包。设备接口、旧端兼容及重启限制见 [采集端上传结果确认](docs/collector-upload-confirmation.md)。

打包入口执行测试、类型与格式检查、Clippy、前端构建、Windows Release 构建及桌面验收；`-IncludeRealEnvironment` 启用真实验收。可通过 `-PackageName` 指定全新交付目录，`-Cargo`、`-Node` 指定本机工具。交付目录仅含一个 `WeComArchive.exe`，检查版本、x64 架构和哈希；不覆盖旧包，不生成验收报告，不执行 Git 提交或对外发布。

## 运行数据

桌面“设置 → 归档存储”可退出工作台并打开独立的备份与恢复窗口；无法正常启动时可运行 `WeComArchive.exe --maintenance`。支持完整备份、校验及恢复到新目录，恢复首次启动须确认并暂停计划及旧采集端。完整范围、操作和 DPAPI 跨机限制见 [备份与恢复](docs/backup-recovery.md)。完整 JSON 导出不包含服务端附件字节，不能替代完整备份。

导入的全部批次与人员、会话目录现在统一事务提交，失败可幂等重试；已有附件同时核对实际哈希。数据库回滚与附件文件的边界见 [导入可靠性](docs/import-reliability.md)。

- 自动发现合并企业微信注册表配置、记住的目录及原有默认目录；优先当前账号数据，历史 `Backup` 数据作为后备候选。工作台、采集端发现和后台任务复用记忆目录。
- 手动填写目录重试时只扫描该目录，避免默认目录中的其他账号被误选。目录须是本机磁盘上的子目录；不接受整盘根目录、网络路径或父目录跳转。目录配置缺失、损坏或失效时，自动发现仍可回退到默认目录。
- 手动确认过的目录以当前 Windows 用户的 DPAPI 保护，保存在 exe 同级 `source-roots.dpapi`，最多保存 3 条；移动程序时需一并保留此文件，换 Windows 用户或电脑后需重新填写。
- 工作台使用 exe 同级 `serverData/` 保存配置、归档、媒体、采集端和导出文件；采集端使用 exe 同级 `collectorData/` 保存运行配置和离线导出。
- 工作台本机采集和快速导出在 `serverData/work/<run-id>/` 处理只读快照；配置 `WECOM_ARCHIVE_SERVER_DATA` 时使用该服务端数据目录。采集端使用 `collectorData/work/<run-id>/`，任务完成或失败后清理。仅使用工作台不会创建 `collectorData/`。采集端尾部携带签名绑定配置，最新有效配置以 DPAPI 保护保存在 `collectorData/instances/<collectorId>/`。

日志默认关闭，可在“生成默认配置”和“采集端管理”中启用采集端日志；环境变量 `WECOM_ARCHIVE_COLLECTOR_LOG`、`WECOM_ARCHIVE_SERVER_LOG` 可设置启动默认值。日志按日期分文件，时间格式为 `yyyy-MM-dd HH:mm:ss.SSS`。

升级说明和人工验收步骤见 [`docs/update-test-plan.md`](docs/update-test-plan.md)。工作台仅在“设置 → 关于”手动检查 HTTPS Release；采集端由管理员在“采集端管理”发起签名升级。两条路径都会校验版本、SHA-256 和 Windows x64 EXE，并保留数据与采集端绑定配置；默认不会后台联网。

| 日志 | exe 同级目录 |
| --- | --- |
| 采集端本地日志 | `collectorData/logs/YYYY-MM-DD.log` |
| 服务端自身日志 | `serverData/logs/server/YYYY-MM-DD.log` |
| 服务端接收采集端日志 | `serverData/logs/collector/<collectorId>/YYYY-MM-DD.log` |

管理页展示实例对应的服务端日志目录；名称或 IP 变更不影响关联。

## 安全边界

工作台默认仅监听本机。已生成采集端按签名绑定配置向已认证服务端同步心跳；自动上传须启用采集计划，也可由用户手动上传。外网地址应使用 HTTPS。工作台仅在用户操作时检查更新；程序不包含遥测、远程字体或第三方资源，日志不记录聊天正文、凭据、密钥或私人路径。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
