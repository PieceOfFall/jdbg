# java-agent-debugger (`jdbg`) — 设计与实现文档

## 1. 项目概述

`jdbg` 是一个跨平台 **Rust CLI 工具**，让 AI 编程 Agent（如 Claude Code）能**在任意 Java 项目中交互式调试程序**，
以 **Windows 为首要目标**平台。

它替代了参考项目 `jdb-agentic-debugger/`（Unix-only 的 Bash 脚本，用 sleep 定时驱动 jdb、不解析输出），
解决两个核心问题：
- **原生跨平台**——纯 Rust，无 Bash/WSL/temp-file 依赖，Windows 原生运行。
- **Prompt-aware，非 sleep-based**——读取 jdb 输出直到 prompt 返回才判定命令完成，超时不杀进程。

## 2. 架构设计

### 2.1 三角色模型

```
  Claude Code tool call          (每次调用 = 一个短命进程)
        │  jdbg break-at Main 9
        ▼
  ┌─────────────┐   local socket (命名管道)   ┌──────────────────────────────────┐
  │  CLI (jdbg) │ ─────────────────────────►  │  Daemon (jdbg __daemon)           │
  │  one-shot   │ ◄─────────────────────────  │  每用户一个，长驻后台              │
  └─────────────┘     JSONL request/response   │  HashMap<SessionId, Session>     │
                                                └──────────────┬───────────────────┘
                                                               │ owns
                                                  ┌────────────┴────────────┐
                                                  ▼                         ▼
                                           ┌─────────────┐           ┌─────────────┐
                                           │ jdb child A │           │ jdb child B │
                                           │  → JVM A    │           │  → JVM B    │
                                           └─────────────┘           └─────────────┘
```

- **CLI** (`jdbg <subcommand>`)：短命进程，解析 clap 参数 → 连接 daemon → 发送 Request → 打印 Response → 退出。
- **Daemon** (`jdbg __daemon`，隐藏子命令)：长驻后台，持有 IPC listener 和 `SessionManager`。
  CLI 首次连接失败时**自动拉起**（detached），无需手动启动。
- **jdb 子进程**：每个调试会话一个，由 daemon 内的 `Session` 拥有。

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
| cli | `src/cli.rs` | clap derive：完整 §7 命令面 |
| output | `src/output.rs` | 人类可读文本渲染 + `--json` 模式 |

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
| 6. SKILL.md + plugin manifest | ❌ 未实现 | Claude 打包：native-first skill 文档 + plugin.json |

## 7. 未实现 / TODO 项

### 7.1 Roadmap 未完成

- **SKILL.md**：文档化 `jdbg` 命令面和调试工作流，供 Claude 参考。
- **`.claude-plugin/plugin.json` + `marketplace.json`**：插件声明（`allowed-tools: Bash(jdbg:*) Read`）。

### 7.2 功能级 TODO

| 功能 | 优先级 | 说明 |
|------|--------|------|
| `dump` 输出解析 | 低 | 当前对复杂对象的 dump 回退为 Raw |
| graceful daemon shutdown | 低 | 当前用 `process::exit(0)`，可改为 shutdown flag |
| Unix setsid detach | 低 | 当前 Unix detach 只靠 stdio null（Windows 完善，Unix 最小可用） |
| 集成测试 | 中 | 需要 JDK 的 feature-gated 集成测试（当前只有单元测试 + 手动验证） |

### 7.3 已完成（本轮）

| 功能 | 说明 |
|------|------|
| **Attach 模式** | `Session::attach` + `process::spawn_attach` + `manager::create_attach`，handler 顶层路由。**关键修复：用 `-connect com.sun.jdi.SocketAttach:hostname=H,port=P` 而非 `jdb -attach host:port`**——Windows 上 `-attach` 默认走共享内存(dt_shmem)，与 JDWP dt_socket 不匹配会 attach 失败、jdb 立即退出。attach 后排空 `VM Started` 异步 banner 避免输出滞后；失败路径捕获 stderr 报错。`run` 在 attach 模式被拒绝。 |
| jdkpath 常见目录扫描 | `find_jdb` 第 4 步扫描 `Program Files\Java\*`、`.jdks\*`、Eclipse Adoptium、Microsoft、`/usr/lib/jvm/*`、macOS bundle 布局；纯 `std::fs`，无新依赖。 |
| `where --all` 多线程栈 | 新增 `CommandResult::ThreadStackTrace { threads: Vec<ThreadStack> }`，parser 按线程 header 分组（`parse_where_all`），output 渲染。 |
| `catch` 异常 thread 推断 | reader 从尾部 thread-prompt 提取线程名回填 `DetectedEvent::Exception.thread`（不再为空串）。 |
| `--timeout` 传递 | `Request.timeout` → handler → `CommandKind::with_timeout_secs` → `read_until_prompt`，便捷方法全部透传。 |

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
