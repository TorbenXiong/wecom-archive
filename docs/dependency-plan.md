# 首次依赖解析清单

本文记录首轮 manifest 中的直接依赖。所有版本均精确锁定；执行 `pnpm install` 或 `cargo fetch/build/test` 后，还会由注册表解析传递依赖并生成锁文件。

## npm registry (`https://registry.npmjs.org`)

运行依赖：

| 包 | 版本 | 用途 |
| --- | --- | --- |
| `react` | `19.2.8` | UI |
| `react-dom` | `19.2.8` | DOM 渲染 |
| `@tauri-apps/api` | `2.11.1` | 类型化 IPC |
| `lucide-react` | `1.42.0` | 独立开源图标 |

开发依赖：

| 包 | 版本 | 用途 |
| --- | --- | --- |
| `@tauri-apps/cli` | `2.11.4` | Tauri 开发与构建 |
| `vite` | `8.2.2` | 前端构建 |
| `@vitejs/plugin-react` | `6.1.1` | React 编译插件 |
| `typescript` | `7.0.2` | 类型检查 |
| `vitest` | `5.0.0` | 单元测试 |
| `jsdom` | `30.0.1` | 测试 DOM |
| `@testing-library/dom` | `10.4.1` | DOM 测试 peer dependency |
| `@testing-library/react` | `16.3.3` | 组件测试 |
| `@testing-library/jest-dom` | `7.0.1` | DOM 断言 |
| `@types/node` | `26.4.1` | Vite 配置的 Node 类型 |
| `@types/react` | `19.2.18` | React 类型 |
| `@types/react-dom` | `19.2.7` | React DOM 类型 |

解析将创建根目录 `pnpm-lock.yaml` 和 `node_modules/`，写入 pnpm store 缓存；不会修改全局安装或系统 `PATH`。

## crates.io (`https://crates.io` / `https://static.crates.io`)

直接依赖：

| crate | 版本 | 用途 |
| --- | --- | --- |
| `tauri` | `2.11.5` | 桌面运行时 |
| `tauri-build` | `2.6.3` | Tauri 构建 |
| `axum` | `0.8.9` | 服务端 JSON 导入 API |
| `tokio` | `1.53.1` | 服务端异步运行时 |
| `tower-http` | `0.7.1` | 同源静态服务端 UI |
| `serde` | `1.0.229` | 契约序列化 |
| `serde_json` | `1.0.151` | JSON 与原始载荷 |
| `thiserror` | `2.0.20` | 类型化错误 |
| `uuid` | `1.26.0` | 稳定批次/任务 ID |
| `chrono` | `0.4.45` | 时间 |
| `sha2` | `0.11.0` | 内容寻址与 manifest hash |
| `hex` | `0.4.3` | hash 编码 |
| `rusqlite` | `0.40.2` | 归档 SQLite/FTS5 |
| `zip` | `8.6.0` | 全量导出 ZIP |
| `csv` | `1.4.0` | CSV 流式输出 |
| `futures-util` | `0.3.34` | 服务端流式接收客户端 JSON |
| `walkdir` | `2.5.0` | 受控目录扫描 |
| `windows` | `0.62.2` | Windows DPAPI 等 API |
| `zeroize` | `1.9.0` | 敏感内存清零 |
| `tempfile` | `3.27.0` | 安全 staging 与测试夹具 |

解析将创建根目录 `Cargo.lock`，下载 crate 源码到当前用户 Cargo 缓存；构建还会写入 `target/`。不会修改系统或用户环境变量。

## 原生加密库（尚不在本轮解析）

SQLite3MultipleCiphers `2.5.1` 已从官方发布包
`sqlite3mc-2.5.1-sqlite-3.53.4-amalgamation.zip` 获取，并以
`4125f8ff275ea953dabb3289331b20a0e76d4fc060f57148f4a5df3bf3b0d5e0`
校验。构建通过本地修补的 `libsqlite3-sys 0.38.2` 维持单一 SQLite
符号提供者，AES-128-CBC 支持静态进入客户端和服务端二进制，不需要外部
DLL。来源、逐文件摘要和两份 MIT 许可位于
`third_party/libsqlite3-sys/`。
