# java-agent-debugger (`jdbg`) — 设计与实现文档

## 1. 项目概述

`jdbg` 是一个跨平台 **Rust CLI 工具**，让 AI 编程 Agent（如 Claude Code）能**在任意 Java 项目中交互式调试程序**，
以 **Windows 为首要目标**平台。

它替代了参考项目 `jdb-agentic-debugger/`（Unix-only 的 Bash 脚本，用 sleep 定时驱动 jdb、不解析输出），
解决两个核心问题：
- **原生跨平台**——纯 Rust，无 Bash/WSL/temp-file 依赖，Windows 原生运行。
- **Prompt-aware，非 sleep-based**——读取 jdb 输出直到 prompt 返回才判定命令完成，超时不杀进程。

**两种接入方式**（共用同一套 daemon/session/jdb 引擎）：
- **CLI**——`jdbg <subcommand>`，人类/脚本直接调用，输出人类文本或 `--json`。
- **MCP server**——`jdbg __mcp`，作为 stdio MCP server 被 Claude Code 识别为**原生工具调用**
  （`mcp__jdbg__launch` 等 25 个工具），无需经 Bash。见 §2.5。

## 2. 架构设计

### 2.1 角色模型（两种客户端 → 一个 daemon → N × jdb）

```
                         Claude Code
              ┌───────────────┴────────────────┐
        Bash  ▼                                 ▼  stdio (JSON-RPC 2.0)
       ┌──────────────┐               ┌──────────────────────┐
       │  CLI (jdbg)  │               │  MCP server          │
       │   one-shot   │               │  (jdbg __mcp)        │
       └──────┬───────┘               └──────────┬───────────┘
              │        client::send_request(&Request)
              └────────────────┬────────────────┘
                               ▼  命名管道 LocalSocket · JSONL (1 Req / 1 Resp)
              ┌────────────────────────────────────┐
              │  Daemon (jdbg __daemon)            │
              │  IPC listener + SessionManager     │
              │  HashMap<SessionId, Session>       │
              └────────────────┬───────────────────┘
                               │ owns
                  ┌────────────┴────────────┐
                  ▼                         ▼
           ┌─────────────┐           ┌─────────────┐
           │ jdb child A │           │ jdb child B │   … N 个并发会话
           │  → JVM A    │           │  → JVM B    │
           └─────────────┘           └─────────────┘
```

`jdbg` 进程有四类角色，Claude Code（或人类终端）从两条客户端路径之一接入，最终汇入同一个 daemon：

- **CLI** (`jdbg <subcommand>`)：短命进程，解析 clap 参数 → 经 `client::send_request` 发 Request → 打印
  Response → 退出。Claude 的每次 Bash 调用对应一个。
- **MCP server** (`jdbg __mcp`，隐藏子命令)：Claude Code 通过 stdio 拉起的进程，把 `tools/call` 翻成
  `Request`、经**同一个** `client::send_request` 发给 daemon——与 CLI 平级的第二种客户端。详见 §2.5。
- **Daemon** (`jdbg __daemon`，隐藏子命令)：长驻后台，持有 IPC listener 和 `SessionManager`。
  CLI / MCP server 首次连接失败时**自动拉起**（detached），无需手动启动。
- **jdb 子进程**：每个调试会话一个，由 daemon 内的 `Session` 拥有；daemon 多路复用 N 个并发会话。

### 2.2 IPC 协议

- 传输层：`interprocess` crate 的 `LocalSocket`（Windows 命名管道 `\\.\pipe\jdbg-<user>`，Unix 抽象 socket）。
- Wire 格式：JSONL（一行 JSON + newline），每连接一个 Request + 一个 Response。
- Socket name 固定于用户名，CLI 无需发现。

### 2.3 jdb 控制引擎

**核心设计决策**：
- 用 `std::process::Command` piped stdin/stdout/stderr 驱动 jdb（**不用 ConPTY**）。
- 始终强制英文 locale（`-J-Duser.language=en -J-Duser.country=US -J-Dfile.encoding=UTF-8`），
  否则本机 jdb 输出乱码中文导致解析失败。
- **Prompt-aware**：逐字节读 stdout 进滚动缓冲区，在尾部匹配 prompt regex（裸 `> ` 或 `thread[frame] `），
  结合事件 banner（Breakpoint hit / Step completed / Exception / VM exit）判定命令完成。
- **ReadMode 区分**：Normal 命令（locals/where/print…）任何 prompt 即完成；
  Blocking 命令（run/cont/step/next/step-out）忽略中间裸 prompt，等 thread-prompt 或事件。
- **超时不杀进程**——返回部分输出 + 标记 `Running`（应用可能死锁/长循环），会话保持存活。

### 2.4 分层架构（高内聚低耦合）

```
层级（上层依赖下层，不反向）：

  bin (main.rs)          ← CLI 入口，仅依赖 lib 公共 API
    │
  cli.rs / output.rs     ← 命令解析 + 渲染（可独立替换 UI）
    │
  client.rs              ← 连接 daemon 的 RPC 客户端
  daemon/                ← IPC 监听 + 会话路由
    │
  session.rs             ← 协调层：状态机 + 命令锁 + 语义方法
    │
  jdb/                   ← 引擎子系统（spawn / 读取 / 解析）
  jdkpath.rs             ← 定位 jdb
    │
  error.rs / protocol.rs ← 基础类型（零内部依赖）
  registry.rs            ← 磁盘注册表
```

### 2.5 MCP server（Claude Code 原生工具）

MCP server 是 **daemon 的第二种客户端**，与 CLI 平级——CLI 把 clap 解析结果转成 `Request` 发给 daemon；
MCP server 把 `tools/call` 转成 `Request` 发给 daemon。两者共用同一条下游链路，**daemon/session/jdb 引擎/
protocol 零改动**。

```
  Claude Code ──stdio (JSON-RPC 2.0, 行分隔)──►  jdbg __mcp  (Claude 拉起的子进程)
                                                     │  tools/call → protocol::Command + Request
                                                     │  client::send_request(&Request)
                                                     ▼
                                           命名管道 ──► daemon ──► Session ──► jdb → JVM
```

- **传输**：手写极简 JSON-RPC 2.0 over stdio（无 tokio，复用 serde_json + blocking IO，与既有 JSONL IPC 同构）。
  实现的方法：`initialize` / `notifications/initialized` / `tools/list` / `tools/call` / `ping`。
- **工具粒度**：细粒度 1:1——每个 jdbg 子命令一个工具，共 25 个（不暴露 daemon 控制；`daemon start/stop/status`
  不该交给 LLM，且 auto-spawn 已覆盖）。`session`/`timeout` 作为通用可选参数注入相关工具的 inputSchema。
- **结果映射**：`Response.ok` → `output::render` 文本塞进 `CallToolResult.content`（`isError:false`）；
  业务错误/daemon 连接失败 → tool-level error（`isError:true`，Claude 可见并继续）；仅协议层问题
  （未知工具、缺必填参数、JSON 解析失败）才用 JSON-RPC error（`-32601`/`-32602`/`-32700`）。
- **stdout 纪律**：stdout 只承载 JSON-RPC，所有日志走 stderr（`eprintln!`），否则污染协议流。
- **接入配置**：开发期 `.mcp.json` 指向 `target/debug/jdbg.exe __mcp`；分发期 `.claude-plugin/plugin.json`
  内联 `mcpServers` 指向 `${CLAUDE_PLUGIN_ROOT}/bin/jdbg`。工具呈现为 `mcp__jdbg__<tool>`
  （plugin 打包时为 `mcp__plugin_java-agent-debugger_jdbg__<tool>`）。

## 3. 模块说明

| 模块 | 文件 | 职责 |
|------|------|------|
| error | `src/error.rs` | `thiserror` Error enum + exit code 映射 |
| protocol | `src/protocol.rs` | `CommandResult`（§8 输出 schema）+ IPC wire 类型 (Request/Response/Command) |
| jdkpath | `src/jdkpath.rs` | 定位 jdb：`--jdb-path` → JAVA_HOME → PATH |
| jdb/process | `src/jdb/process.rs` | Spawn jdb (piped, MANDATORY -J flags)，`write_command` |
| jdb/reader | `src/jdb/reader.rs` | Prompt-aware 读取器：Normal/Blocking 模式、event 检测、超时 |
| jdb/parser | `src/jdb/parser.rs` | 将原始文本分类解析为 `CommandResult`（正则驱动，TDD 验证） |
| session | `src/session.rs` | Session 拥有 jdb child + reader + stderr drain；RunState 状态机；命令锁；语义方法 |
| daemon | `src/daemon/mod.rs` | daemon 生命周期：bind socket、accept loop、detach spawn helper |
| daemon/handler | `src/daemon/handler.rs` | 单连接处理：解码 Request → 路由 → 编码 Response |
| daemon/manager | `src/daemon/manager.rs` | `SessionManager`：HashMap 管理多会话、create/get/list/kill |
| registry | `src/registry.rs` | `directories` 定位数据目录；原子写 daemon.json / sessions.json |
| client | `src/client.rs` | connect-or-auto-spawn，发一条 Request 收一条 Response |
| cli | `src/cli.rs` | clap derive：完整 §7 命令面 + 隐藏 `__daemon` / `__mcp` 入口 |
| output | `src/output.rs` | 人类可读文本渲染 + `--json` 模式（返回 String，MCP 层复用） |
| mcp | `src/mcp/mod.rs` | MCP server `run_mcp()`：stdio JSON-RPC 主循环 + 生命周期 + 结果映射 |
| mcp/jsonrpc | `src/mcp/jsonrpc.rs` | JSON-RPC 2.0 请求/响应/错误类型 + 标准错误码 |
| mcp/tools | `src/mcp/tools.rs` | 25 工具 spec（name/description/inputSchema）+ `dispatch_tool` 工具→Command 翻译层 |

## 4. CLI 命令面

```
jdbg launch <MainClass> [--classpath CP] [--sourcepath SP] [--name N] [-- app-args...]
jdbg attach [--host H] [--port P] [--sourcepath SP] [--name N]
jdbg status | list | kill [--session ID]
jdbg daemon start|stop|status

jdbg break-at <Class> <line>
jdbg break-in <Class> <method> [--args types]
jdbg catch <Exception> [--mode caught|uncaught|all]
jdbg breakpoints | clear <spec>

jdbg run | cont | step | next | step-out

jdbg where [--all] | locals | print <expr> | dump <obj> | eval <expr>
jdbg threads | thread <id> | frame <up|down> [n] | list-source [line]
jdbg raw <jdb command...>

全局参数：--session <id> --json --timeout <secs> --jdb-path <path>
```

**MCP 工具面**：上述子命令 1:1 映射为 25 个 MCP 工具（`launch`/`attach`/`status`/`list`/`kill`/
`break_at`/`break_in`/`catch`/`breakpoints`/`clear`/`run`/`cont`/`step`/`next`/`step_out`/`where`/
`locals`/`print`/`dump`/`eval`/`threads`/`thread`/`frame`/`list_source`/`raw`），命名用 snake_case，
参数用 JSON object。`daemon` 控制命令不暴露为工具。详见 §2.5。

## 5. RunState 状态机

```
Loaded  ──run──►  Suspended  ──cont/step/next──►  Suspended
                      │                                 │
                      └────────cont(到结束)──►  Exited
                                                        │
                 Fatal/EOF ──────────────────►  Dead
                 Timeout ────────────────────►  Running（不杀进程）
```

## 6. 已实现功能

| Roadmap 步骤 | 状态 | 说明 |
|--------------|------|------|
| 1. protocol + error | ✅ 完成 | 完整 CommandResult enum + Error + exit codes |
| 2. jdb 引擎 (process/reader/parser) | ✅ 完成 | prompt-aware 读取、TDD parser (16 测试)、真实 jdb 验证 |
| 3. session 层 | ✅ 完成 | 三线程模型、RunState、命令锁、语义便捷方法、launch + attach |
| 4. daemon + IPC + client + registry | ✅ 完成 | interprocess 命名管道、auto-spawn、SessionManager、磁盘注册表 |
| 5. cli.rs + output.rs | ✅ 完成 | clap 完整命令面 + 文本/JSON 渲染 |
| 6. SKILL.md + plugin manifest | ✅ 完成 | native-first `skills/jdbg/SKILL.md` + `.claude-plugin/{plugin,marketplace}.json`，subagent 应用场景验证通过 |
| 7. MCP server | ✅ 完成 | `src/mcp/{mod,jsonrpc,tools}.rs`：手写 stdio JSON-RPC、25 工具 1:1 映射、`.mcp.json` + plugin 内联 mcpServers、SKILL.md 改写为 MCP 工具面；真实 jdb e2e 验证（launch→break→run→locals→cont） |

## 7. 未实现 / TODO 项

### 7.1 功能级 TODO

| 功能 | 优先级 | 说明 |
|------|--------|------|
| `dump` 输出解析 | 低 | 当前对复杂对象的 dump 回退为 Raw |
| graceful daemon shutdown | 低 | 当前用 `process::exit(0)`，可改为 shutdown flag |
| Unix setsid detach | 低 | 当前 Unix detach 只靠 stdio null（Windows 完善，Unix 最小可用） |
| 集成测试 | 中 | 需要 JDK 的 feature-gated 集成测试（当前只有单元测试 + 手动/e2e 验证） |
| MCP plugin 跨平台二进制名 | 中 | `plugin.json` 的 `${CLAUDE_PLUGIN_ROOT}/bin/jdbg` 在 Windows 需 `.exe`，plugin.json 无法按平台分支；打包阶段需按平台放对应二进制。开发期用 `.mcp.json` 不受影响 |

### 7.2 已完成（历史轮次）

| 功能 | 说明 |
|------|------|
| **MCP server（本轮）** | `src/mcp/{jsonrpc,tools,mod}.rs`：手写极简 stdio JSON-RPC 2.0（无 tokio，零新增依赖），25 工具 1:1 映射现有子命令，复用 `client::send_request` + `output::render`，daemon/session/jdb 零改动。`.mcp.json` + `plugin.json` 内联 mcpServers，SKILL.md 改写为 MCP 工具面（保留何时用/react-to-each-result）。21 个新单测 + 真实 jdb e2e。**关键修复：Windows 句柄继承泄漏**——MCP server `run_mcp()` 入口用零依赖 `SetHandleInformation` 裸 FFI 清除自身 stdout/stderr 的 `HANDLE_FLAG_INHERIT`，否则 auto-spawn 的 detached daemon 继承 MCP 的 stdout 管道写端，使 Claude 端读不到 EOF。 |
| **Roadmap 6: SKILL.md + plugin** | native-first `skills/jdbg/SKILL.md`（命令面 §7、stateful "react to each result" 工作流、JDWP 版本感知启用、`-g` 提示、attach；剔除参考的 WSL/temp/sleep/`--auto-inspect`）+ `.claude-plugin/{plugin,marketplace}.json`。subagent 应用场景验证通过（仅凭 skill 正确驱动 launch→break→run→inspect）。 |
| **Attach 模式** | `Session::attach` + `process::spawn_attach` + `manager::create_attach`，handler 顶层路由。**关键修复：用 `-connect com.sun.jdi.SocketAttach:hostname=H,port=P` 而非 `jdb -attach host:port`**——Windows 上 `-attach` 默认走共享内存(dt_shmem)，与 JDWP dt_socket 不匹配会 attach 失败、jdb 立即退出。attach 后排空 `VM Started` 异步 banner 避免输出滞后；失败路径捕获 stderr 报错。`run` 在 attach 模式被拒绝。 |
| jdkpath 常见目录扫描 | `find_jdb` 第 4 步扫描 `Program Files\Java\*`、`.jdks\*`、Eclipse Adoptium、Microsoft、`/usr/lib/jvm/*`、macOS bundle 布局；纯 `std::fs`，无新依赖。 |
| `where --all` 多线程栈 | 新增 `CommandResult::ThreadStackTrace { threads: Vec<ThreadStack> }`，parser 按线程 header 分组（`parse_where_all`），output 渲染。 |
| `catch` 异常 thread 推断 | reader 从尾部 thread-prompt 提取线程名回填 `DetectedEvent::Exception.thread`（不再为空串）。 |
| `--timeout` 传递 | `Request.timeout` → handler → `CommandKind::with_timeout_secs` → `read_until_prompt`，便捷方法全部透传。 |
| `kill` 默认会话 | `jdbg kill` 不带 `--session` 时默认唯一存活会话（与其它命令一致，§7 全局标志约定）。 |

## 8. 依赖清单

| Crate | 版本 | 用途 |
|-------|------|------|
| clap (derive) | 4 | CLI 命令解析 |
| serde + serde_json | 1 | JSONL wire 格式 + `--json` 输出 |
| interprocess | 2 | 跨平台本地 socket（Windows 命名管道 / Unix domain socket） |
| thiserror | 2 | 错误类型 |
| anyhow | 1 | 顶层错误 context |
| regex | 1 | jdb 输出 prompt/event 匹配 |
| directories | 6 | 平台数据目录定位 |
| rand | 0.9 | 生成 session ID |
| jiff | 0.2 | 时间戳 |

> MCP server **零新增依赖**：手写 JSON-RPC（serde_json）、Windows 句柄修复用 `std` 裸 FFI（kernel32
> `SetHandleInformation`，无 windows/winapi crate）。刻意排除 `rmcp`（会引入 tokio）、`tokio`、`once_cell`、
> `nix`、`daemonize`、ConPTY、TCP/RPC 框架。

## 9. 构建与运行

```bash
# 构建
cargo build

# 运行测试
cargo test

# 使用（daemon 自动拉起）
./target/debug/jdbg launch Main --classpath . --sourcepath src

# 手动控制 daemon
./target/debug/jdbg daemon start
./target/debug/jdbg daemon status
./target/debug/jdbg daemon stop
```

## 10. 已验证的端到端流程

以下流程在 Windows 11 + Zulu JDK 1.8.0_492 上验证通过：

```
jdbg launch Main --classpath fixtures     → SessionCreated (loaded)
jdbg break-at Main 9                      → BreakpointSet (deferred)
jdbg run                                  → Stopped (breakpoint @ Main:9)
jdbg locals                               → 4 vars (args, count, label, sum)
jdbg where                                → [1] Main.main (Main.java:9)
jdbg print count                          → count = 3
jdbg threads                              → 5 threads in 2 groups
jdbg list-source                          → source with => marker at line 9
jdbg step                                 → Stopped (step @ line 10)
jdbg cont                                 → VmExited
jdbg status                               → state=exited
jdbg daemon stop                          → Daemon stopped
```

**关键验证点**：
- ✅ 每个 `jdbg` 调用是独立进程，会话在 daemon 后台存活（跨进程）
- ✅ 无 sleep——全部靠 prompt 检测判定命令完成
- ✅ 阻塞命令（run/step/cont）正确跳过中间裸 prompt，等待事件
- ✅ locale 强制生效（jdb 输出英文）
- ✅ 文本渲染人类可读、JSON 渲染机器可解析

**MCP 路径 e2e**（同机，喂 JSON-RPC 到 `jdbg __mcp` stdin）：

```
initialize                                → serverInfo=jdbg, capabilities.tools
tools/list                                → 25 工具，每个 inputSchema.type=object
tools/call launch {main_class,classpath}  → Session created (loaded)
tools/call break_at {class,line:9}        → Breakpoint set (deferred)
tools/call run {timeout:30}               → Breakpoint hit: Main.main() line=9
tools/call locals                         → count=3, label="hello", sum=3
tools/call cont                           → The application exited
```

- ✅ 完整链路 Claude → MCP server → daemon → jdb → JVM，结构化结果正确（1.3s 完成）
- ✅ 句柄修复后 auto-spawn daemon 不再泄漏 stdout 管道，进程干净退出
- ✅ 37 个库单测全绿（16 既有 + 21 MCP，零破坏）
