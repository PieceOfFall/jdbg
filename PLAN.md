# jdbg 改进计划（PLAN）

> 来源：一次真实的外部 Tomcat（SSM + Dubbo + Druid，suspend=n，JDK 8）attach 调试实战，
> 暴露出 reader 时序脆弱、整服冻结、attach 重连不可靠、源码乱码等问题。
> 本文件记录待办改进点，每点附**具体场景**、**根因**、**方案**、**风险**。

---

## 1. suspend policy 可配置（只挂命中线程）

### 场景
调试一个在线 Tomcat：用户对 `CartYzAdapter.getAllCartAndDemandDataV2` 打断点。命中的瞬间，
**整个 Tomcat 的所有线程被挂起**——其他用户的请求全部卡死、健康检查超时、可能被网关摘除。
用户只想看一个请求的局部变量，却把整台服务器冻住了。

### 根因
jdb 的 `stop at` / `stop in` 设断点时默认 **SUSPEND_ALL**（命中即挂起 VM 全部线程）。
当前 `src/session.rs` 的 `stop_at`/`stop_in`（行 293-303）直接发 `stop at Class:line`，
无法指定 suspend policy。

### 方案
- 给 `break-at` / `break-in` 增加可选参数 `suspend_policy`（`all` | `thread`，attach 模式默认 `thread`）。
- **先验证 jdb 是否支持线程级 suspend**：原生 `jdb` 的 `stop` 命令语法**不直接暴露** suspend policy
  （不像 JDI 的 `EventRequest.setSuspendPolicy`）。需先用裸 jdb 实测：
  - 选项 A：`stop` 命中后，jdb 实际只让命中线程停、其他继续？（实测当前是 ALL）
  - 选项 B:若 jdb 无法做线程级，则此特性受限于 jdb 能力，需在文档说明，或考虑 `monitor` 等变通。
- 若 jdb 确实只能 ALL：退而求其次——命中后**立即自动 resume 其他线程**（命中线程保持挂起），
  用 `thread <hex-id>` 锁定 + `resume`（不带参恢复全部，再单独处理）的组合模拟线程级。需谨慎验证。

### 风险
中。依赖 jdb 自身能力，可能无法做到真正的线程级 suspend。**动手前必须先用裸 jdb 做可行性实测**，
不可假设 jdb 支持。

---

## 2. reader sentinel 根治时序竞态

### 场景
attach + suspend=n 下，`cont` 命中断点后紧接着 `where` / `locals`，间歇性返回
`No thread specified` / 空响应。当前已用 `drain_stale`(c5dde8e) + `purge_pending`(1dd9b4a)
两个补丁缓解，但本质是**赌时序**——高并发的真实服务器（100+ 线程）下仍可能漏。

### 根因
`src/jdb/reader.rs` 的 `read_until_prompt` 模型是"匹配到 prompt 就返回"。但 jdb 的输出是异步的：
- thread-prompt 之后会追加迟到的 bare prompt `> `（cont 对 running VM 的延迟响应）
- 事件 banner 与 prompt 可能乱序交错
"猜命令输出边界"在并发下不可靠。drain/purge 是用固定等待窗口去清残留，不是确定性的。

### 方案
引入 **sentinel 模式**（GDB/MI、DAP adapter 的通行做法）：
- 每条命令执行后，紧跟一条注入命令产生唯一标记，如 `print "JDBG_DONE_<nonce>"`。
- reader 不再以 prompt 为完成信号，而是**读到 sentinel 的回显**才算命令真正结束。
- 这把"猜边界"变成"等确定标记"，彻底消除迟到 prompt / 乱序输出导致的错位。
- 需保留对 blocking 命令（run/cont/step）的特殊处理：sentinel 只在命令真正返回 prompt 后注入。

### 风险
高。这是 CLAUDE.md 明列的**最高风险区**（reader/parser 是正确性核心）。
- 必须先攒足真实 jdb transcript 作为测试 oracle（覆盖 launch/attach、单/多线程、断点/异常/单步）。
- 建议单独立项、单独 PR、充分回归后再合入。
- sentinel 命令本身的输出也要从结果中剥离，不能污染用户可见输出。

---

## 3. attach 健康探测 + 源码编码

### 场景 A（attach 诊断）
用户 attach 到一个端口（写错端口、JDWP 没开、或 server=n），jdbg 当前可能给出裸 timeout 或
晦涩的 jdb 报错，用户不知道是"端口错了"还是"JVM 没开 JDWP"。

### 场景 B（源码乱码）
目标 Tomcat 以 `-Dfile.encoding=GBK` 启动，SSM 项目源码是 GBK 编码。jdbg 强制 jdb 用 UTF-8
（`src/jdb/process.rs` 的 `LOCALE_FLAGS`，行 12-15，为了让英文事件 banner 可解析）。
结果 `list-source` 显示的中文源码**乱码**（变量值走 JDWP 不受影响，但看源码定位行号受影响）。

### 根因
- A：attach 前未做端口可达性探测；jdb 连接失败的输出未翻译成清晰诊断。
  现有 `RE_FATAL`（reader.rs 行 52-56）只匹配 `Unable to attach` 等，不含 `Connection refused`。
- B：`LOCALE_FLAGS` 的 UTF-8 是**必须保留**的（事件 banner 解析依赖它），不能改。
  但 `list-source` 读的是源码文件，可以独立按目标编码解码。

### 方案
- **A**：attach 前先 TCP 探测 host:port，不可达直接给"端口不可达，检查 JDWP 是否启用 / server=y / 端口正确"。
  扩展 `RE_FATAL` 覆盖 `Connection refused` 等常见失败，映射到清晰的 `Error::Connection`。
- **B**：给 `attach` / `launch` 增加可选 `source_encoding` 参数（如 `GBK`）；`list-source` 输出时
  按该编码解码源码行。**注意**：保持 `LOCALE_FLAGS` 的 UTF-8 不变（banner 解析），只在源码展示层转码。

### 风险
低。两项都相对独立，不动 reader 核心逻辑。编码项需注意区分"jdb 协议输出"（UTF-8）和
"源码文件内容"（目标编码）两条路径。

---

## 实施建议（优先级）

1. **#3（健康探测+编码）** — 风险最低、独立、直接改善体验，先做。
2. **#1（suspend policy）** — 性价比高但**需先做 jdb 可行性实测**，确认能力边界后再定方案。
3. **#2（sentinel 重构）** — 最重要但最危险，单独立项，先攒 transcript 测试集再动 reader。

> 通用约束（CLAUDE.md）：reader/parser 改动必须有真实 jdb transcript 佐证；纯逻辑走 TDD；
> 平台副作用用端到端验证。本地已有可复现环境：`C:\Users\luyiwen\jdbg-repro\Server.java`
> （多线程线程池 + suspend=n，可稳定复现 attach 场景）。
