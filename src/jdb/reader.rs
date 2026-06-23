//! Prompt-aware reader：逐字节从 jdb stdout 读取，识别 prompt / 事件 / VM退出。
//!
//! 核心契约（§5）：
//! - 普通命令：写 `cmd\n` 后读到 prompt 即为就绪，小超时（~15s）。
//! - 阻塞命令（run/cont/step/next/step up）：prompt 直到断点命中/异常/VM退出才回来，大超时（~30s）。
//! - 超时不杀进程——返回部分输出 + `ReadOutcome::Timeout`。
//!
//! 实现：
//! - 逐字节读入 `Vec<u8>` 滚动缓冲区（prompt 无 trailing newline，单次 read 不一定含完整行）。
//! - `\r\n` → `\n` 归一化 + UTF-8 lossy 解码后在缓冲区尾部匹配 prompt regex。
//! - 检测 event banner（breakpoint hit / step completed / exception / VM exit）发出语义信号。

use std::io::Read;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

// ─── 正则（§5，LazyLock 一次编译）─────────────────────────────────────────────

/// 裸 prompt `> `——出现在 VM 启动前、以及阻塞命令（run/cont）执行后的**中间态**
/// （VM Started 时短暂出现）。Blocking 模式下必须忽略它，否则会错过随后的断点事件。
static RE_BARE_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^>\s$").unwrap());

/// Thread prompt `thread[frame] `（如 `main[1] `、`Thread-3[2] `、`pool-1-thread-2[2] `）。
/// 出现它代表 VM 真正挂起在某个线程帧——是停下/命令完成的可靠信号。
static RE_THREAD_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\s\[\]]+\[\d+\]\s$").unwrap());

/// Breakpoint hit / Step completed.
/// 真实格式: `Breakpoint hit: "thread=main", Main.main(), line=3 bci=0`
static RE_BREAKPOINT_OR_STEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^(?P<kind>Breakpoint hit|Step completed): "thread=(?P<thread>[^"]+)", (?P<class>\S+)\.(?P<method>\S+)\(\), line=(?P<line>\d+)"#,
    )
    .unwrap()
});

/// Exception occurred.
static RE_EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^Exception occurred: (?P<exc>\S+) \((?P<caught>caught|uncaught)")
        .unwrap()
});

/// VM 退出 / 断开。
static RE_VM_EXIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^The application (?:exited|has been disconnected)").unwrap()
});

/// 连接/启动致命错误。
static RE_FATAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^(?:Unable to attach to target VM|java\.io\.IOException|Input stream closed)",
    )
    .unwrap()
});

// ─── 公开类型 ────────────────────────────────────────────────────────────────────

/// 读取模式：决定如何对待裸 prompt `> `。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// 普通命令（locals/where/print/stop…）：任何 prompt（裸或 thread）都代表命令完成。
    Normal,
    /// 阻塞命令（run/cont/step/next/step-out）：忽略中间的裸 prompt，
    /// 只在 thread-prompt / 事件 banner / VM退出 时才算停下。
    Blocking,
}

/// reader 交给上层的单次读取结果。
#[derive(Debug, Clone)]
pub enum ReadOutcome {
    /// 成功读到 prompt，附带 prompt 之前的全部文本。
    Prompt {
        /// prompt 之前的输出文本（已归一化、去掉尾部 prompt 行本身）。
        output: String,
        /// 检测到的事件（如果有的话）。
        event: Option<DetectedEvent>,
    },
    /// VM 退出。
    VmExit { output: String },
    /// jdb 报告致命错误（连接失败等）。
    Fatal { message: String },
    /// 超时——返回目前已读到的部分输出；**不杀进程**。
    Timeout { partial: String },
    /// stdout EOF（管道关闭 / jdb 退出）。
    Eof { output: String },
}

/// reader 识别的事件语义。
#[derive(Debug, Clone)]
pub enum DetectedEvent {
    Breakpoint {
        thread: String,
        class: String,
        method: String,
        line: u32,
    },
    Step {
        thread: String,
        class: String,
        method: String,
        line: u32,
    },
    Exception {
        thread: String,
        exception: String,
        caught: bool,
    },
}

// ─── PromptReader ────────────────────────────────────────────────────────────────

/// 后台读线程发来的数据块；`None` 表示 EOF。
type Chunk = Option<Vec<u8>>;

/// 从 jdb stdout 读取并识别 prompt/事件的读取器。
///
/// 内部起一个后台线程逐块读取 stdout（标准库阻塞 IO 无法超时，故用线程 + channel），
/// `read_until_prompt` 在主线程用 `recv_timeout` 累积字节并在缓冲区尾部匹配 prompt。
pub struct PromptReader {
    rx: std::sync::mpsc::Receiver<Chunk>,
    /// 原始字节缓冲（跨 chunk 累积，用于正确处理跨界的 `\r\n`）。
    raw: Vec<u8>,
    /// 归一化（`\r\n`→`\n`）+ UTF-8 lossy 解码后的文本，匹配都基于它。
    text: String,
    eof: bool,
    _handle: std::thread::JoinHandle<()>,
}

impl PromptReader {
    /// 接管 stdout，启动后台读线程。
    pub fn new<R: Read + Send + 'static>(mut stdout: R) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Chunk>();
        let handle = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Some(chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                }
            }
        });
        Self {
            rx,
            raw: Vec::new(),
            text: String::new(),
            eof: false,
            _handle: handle,
        }
    }

    /// 读取直到 prompt / 事件 / VM退出 / 致命错误 / EOF / 超时。
    ///
    /// `mode` 决定如何对待裸 prompt（见 [`ReadMode`]）。
    /// 返回后，已消费的文本从内部缓冲清空（下一条命令从干净缓冲开始）。
    pub fn read_until_prompt(&mut self, timeout: Duration, mode: ReadMode) -> ReadOutcome {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(outcome) = self.try_match(mode) {
                return outcome;
            }
            if self.eof {
                return ReadOutcome::Eof {
                    output: self.take_text(),
                };
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return ReadOutcome::Timeout {
                    partial: self.text.clone(),
                };
            };
            match self.rx.recv_timeout(remaining) {
                Ok(Some(bytes)) => self.push(&bytes),
                Ok(None) => self.eof = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return ReadOutcome::Timeout {
                        partial: self.text.clone(),
                    };
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.eof = true,
            }
        }
    }

    /// 追加原始字节，重新归一化解码（缓冲不大，重复解码可接受）。
    fn push(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        let decoded = String::from_utf8_lossy(&self.raw);
        self.text = decoded.replace("\r\n", "\n").replace('\r', "\n");
    }

    /// 取出并清空当前文本缓冲。
    fn take_text(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.raw.clear();
        out
    }

    /// 检查当前缓冲是否到达某个终止条件。
    fn try_match(&mut self, mode: ReadMode) -> Option<ReadOutcome> {
        // 1. 致命错误优先。
        if let Some(m) = RE_FATAL.find(&self.text) {
            let line = current_line(&self.text, m.start()).to_string();
            let _ = self.take_text();
            return Some(ReadOutcome::Fatal { message: line });
        }

        // 2. VM 退出：banner 出现即判定（其后可能 EOF，不一定有 prompt）。
        if RE_VM_EXIT.is_match(&self.text) {
            return Some(ReadOutcome::VmExit {
                output: self.take_text(),
            });
        }

        // 3. 尾部 prompt：判断命令是否完成。
        let last_line = self.text.rsplit('\n').next().unwrap_or("");
        let is_thread_prompt = RE_THREAD_PROMPT.is_match(last_line);
        let is_bare_prompt = RE_BARE_PROMPT.is_match(last_line);

        // thread-prompt 总是代表停下；裸 prompt 只有 Normal 模式才算完成。
        let done = is_thread_prompt || (is_bare_prompt && mode == ReadMode::Normal);
        if done {
            // thread-prompt（如 `main[1] `）携带当前线程名，供无 thread= 的事件 banner（异常）回填。
            let prompt_thread = is_thread_prompt.then(|| thread_from_prompt(last_line)).flatten();
            let cut = self.text.len() - last_line.len();
            let output = self.text[..cut].trim_end_matches('\n').to_string();
            let event = detect_event(&output, prompt_thread.as_deref());
            let _ = self.take_text();
            return Some(ReadOutcome::Prompt { output, event });
        }

        // Blocking 模式下出现裸 prompt：忽略它，继续等待真正的停下信号。
        None
    }
}

/// 在 `output` 文本里检测事件 banner。
/// `prompt_thread`：尾部 thread-prompt 推断出的线程名（异常 banner 不含 thread= 时回填）。
fn detect_event(output: &str, prompt_thread: Option<&str>) -> Option<DetectedEvent> {
    if let Some(c) = RE_BREAKPOINT_OR_STEP.captures(output) {
        let thread = c["thread"].to_string();
        let class = c["class"].to_string();
        let method = c["method"].to_string();
        let line = c["line"].parse().unwrap_or(0);
        let is_step = &c["kind"] == "Step completed";
        return Some(if is_step {
            DetectedEvent::Step { thread, class, method, line }
        } else {
            DetectedEvent::Breakpoint { thread, class, method, line }
        });
    }
    if let Some(c) = RE_EXCEPTION.captures(output) {
        // Exception banner 不含 thread=；从尾部 thread-prompt 推断当前线程（否则留空）。
        return Some(DetectedEvent::Exception {
            thread: prompt_thread.unwrap_or("").to_string(),
            exception: c["exc"].to_string(),
            caught: &c["caught"] == "caught",
        });
    }
    None
}

/// 从 thread-prompt 行（如 `main[1] `）提取线程名（`[` 之前的部分）。
fn thread_from_prompt(line: &str) -> Option<String> {
    if !RE_THREAD_PROMPT.is_match(line) {
        return None;
    }
    line.split('[').next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 取出 `text` 中包含字节偏移 `pos` 的那一整行。
fn current_line(text: &str, pos: usize) -> &str {
    let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[pos..].find('\n').map(|i| pos + i).unwrap_or(text.len());
    &text[start..end]
}
