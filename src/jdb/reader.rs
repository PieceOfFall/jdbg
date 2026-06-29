//! Prompt-aware reader: read jdb stdout byte by byte and recognize prompts, events, and VM exit.
//!
//! Core contract (§5):
//! - Normal commands: after writing `cmd\n`, the command is ready when a prompt is read; short timeout (~15s).
//! - Blocking commands (run/cont/step/next/step up): the prompt returns only at breakpoint/exception/VM exit;
//!   use a longer timeout (~30s).
//! - Do not kill the process on timeout: return partial output + `ReadOutcome::Timeout`.
//!
//! Implementation:
//! - Read byte-wise into a rolling `Vec<u8>` buffer; prompts have no trailing newline and one read may not
//!   contain a complete line.
//! - Normalize `\r\n` → `\n`, decode UTF-8 lossily, and match prompt regexes at the buffer tail.
//! - Detect event banners (breakpoint hit / step completed / exception / VM exit) and emit semantic signals.

use std::io::Read;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

// ─── Regexes (§5, compiled once through LazyLock) ──────────────────────────────

/// Bare prompt `> `, seen before VM startup and as an **intermediate state** after blocking commands
/// (`run`/`cont`) when `VM Started` briefly appears. Blocking mode must ignore it or it will miss the
/// following breakpoint event.
static RE_BARE_PROMPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^>\s$").unwrap());

/// Thread prompt `thread[frame] `, such as `main[1] `, `Thread-3[2] `, `pool-1-thread-2[2] `.
/// Its presence means the VM is truly suspended in a thread frame, a reliable stop/command-complete signal.
static RE_THREAD_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\s\[\]]+\[\d+\]\s$").unwrap());

/// Breakpoint hit / Step completed.
/// Real format: `Breakpoint hit: "thread=main", Main.main(), line=3 bci=0`
/// Note: in en_US locale, jdb emits thousands separators for line numbers >=1000, e.g. `line=3,956`.
static RE_BREAKPOINT_OR_STEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^(?P<kind>Breakpoint hit|Step completed): "thread=(?P<thread>[^"]+)", (?P<class>\S+)\.(?P<method>\S+)\(\), line=(?P<line>[\d,]+)"#,
    )
    .unwrap()
});

/// Exception occurred.
static RE_EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^Exception occurred: (?P<exc>\S+) \((?P<caught>caught|uncaught)").unwrap()
});

/// Field watchpoint hit (real jdb format, JDK 8–21+).
/// Modification: `Field (Cls.field) is <old>, will be <new>: "thread=T", Cls.method(), line=N bci=M`
/// Access:       `Field (Cls.field) is <value>: "thread=T", Cls.method(), line=N bci=M`
/// The detail may contain colons inside quoted values, so match `: "thread=` as the separator.
/// Note: in en_US locale, line may contain thousands separators, e.g. `line=3,956`.
static RE_FIELD_WATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^Field \((?P<field>[^)]+)\) (?P<detail>.+): "thread=(?P<thread>[^"]+)", (?P<class>\S+)\.(?P<method>\S+)\(\), line=(?P<line>[\d,]+)"#,
    )
    .unwrap()
});

/// VM exit / disconnect.
static RE_VM_EXIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^The application (?:exited|has been disconnected)").unwrap());

/// Fatal connection/launch error.
static RE_FATAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^(?:Unable to attach to target VM|java\.io\.IOException|Input stream closed|Connection refused)",
    )
    .unwrap()
});

// ─── Public Types ──────────────────────────────────────────────────────────────

/// Read mode: determines how bare prompt `> ` is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// Normal commands (locals/where/print/stop...): any prompt, bare or thread, completes the command.
    Normal,
    /// Blocking commands (run/cont/step/next/step-out): ignore intermediate bare prompts and stop only at
    /// thread-prompt / event banner / VM exit.
    Blocking,
}

/// Single read result returned by the reader to upper layers.
#[derive(Debug, Clone)]
pub enum ReadOutcome {
    /// Successfully read a prompt, with all text before the prompt.
    Prompt {
        /// Output text before the prompt, normalized and with the trailing prompt line removed.
        output: String,
        /// Detected event, if any.
        event: Option<DetectedEvent>,
    },
    /// VM exited.
    VmExit { output: String },
    /// jdb reported a fatal error, such as connection failure.
    Fatal { message: String },
    /// Timeout: return partial output read so far; **do not kill the process**.
    Timeout { partial: String },
    /// stdout EOF, from pipe close or jdb exit.
    Eof { output: String },
}

/// Event semantics recognized by the reader.
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
    FieldWatch {
        thread: String,
        field: String,
        access_type: String,
        class: String,
        method: String,
        line: u32,
    },
    /// JDK 8 SUSPEND_THREAD truncated banner: jdb writes the `"Breakpoint hit: "` or `"Step completed: "`
    /// prefix and then stops, with no thread/location/thread-prompt. Session must enrich it automatically.
    PartialStop { is_step: bool },
}

// ─── PromptReader ────────────────────────────────────────────────────────────────

/// Truncated-banner patience window: after seeing a truncated prefix, if no further bytes arrive within
/// this duration, classify it as PartialStop. Full SUSPEND_ALL banners arrive within a few ms, so 500ms is conservative.
const PARTIAL_BANNER_PATIENCE: Duration = Duration::from_millis(500);

/// recv_timeout polling granularity during the patience window.
const PARTIAL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Data block from the background reader thread; `None` means EOF.
type Chunk = Option<Vec<u8>>;

/// Reader that reads jdb stdout and recognizes prompts/events.
///
/// Internally starts a background thread to read stdout chunks. Standard-library blocking IO cannot time
/// out directly, so it uses a thread + channel. `read_until_prompt` accumulates bytes in the main thread
/// with `recv_timeout` and matches prompts at the buffer tail.
pub struct PromptReader {
    rx: std::sync::mpsc::Receiver<Chunk>,
    /// Raw byte buffer accumulated across chunks, so split `\r\n` boundaries are handled correctly.
    raw: Vec<u8>,
    /// Normalized (`\r\n`→`\n`) and UTF-8 lossy-decoded text used for all matching.
    text: String,
    eof: bool,
    /// First time a truncated banner (`Breakpoint hit: ` / `Step completed: `, JDK 8 SUSPEND_THREAD)
    /// appeared at the buffer tail. Used for the patience window: only classify as PartialStop after
    /// [`PARTIAL_BANNER_PATIENCE`] passes with no further bytes. New bytes reset it, so full banners are not misclassified.
    partial_banner_since: Option<Instant>,
    _handle: std::thread::JoinHandle<()>,
}

impl PromptReader {
    /// Take ownership of stdout and start the background reader thread.
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
            partial_banner_since: None,
            _handle: handle,
        }
    }

    /// Read until prompt / event / VM exit / fatal error / EOF / timeout.
    ///
    /// `mode` determines how bare prompts are handled (see [`ReadMode`]).
    /// On return, consumed text is removed from the internal buffer so the next command starts cleanly.
    pub fn read_until_prompt(&mut self, timeout: Duration, mode: ReadMode) -> ReadOutcome {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(outcome) = self.try_match(mode) {
                return outcome;
            }

            // Patience window: in Blocking mode the buffer tail is a truncated banner (JDK 8 SUSPEND_THREAD).
            // If no bytes arrive within PARTIAL_BANNER_PATIENCE, classify it as PartialStop; jdb will not write location.
            if mode == ReadMode::Blocking && self.tail_is_partial_banner() {
                match self.partial_banner_since {
                    None => self.partial_banner_since = Some(Instant::now()),
                    Some(since) if since.elapsed() >= PARTIAL_BANNER_PATIENCE => {
                        return self.emit_partial_stop();
                    }
                    Some(_) => {}
                }
            } else {
                // Tail is no longer a truncated banner (full banner continued, or other output), so reset the timer.
                self.partial_banner_since = None;
            }

            if self.eof {
                return ReadOutcome::Eof {
                    output: self.take_text(),
                };
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return ReadOutcome::Timeout {
                    partial: self.take_text(),
                };
            };
            // During the patience window, poll at small granularity so expiry promptly classifies PartialStop.
            let wait = if self.partial_banner_since.is_some() {
                remaining.min(PARTIAL_POLL_INTERVAL)
            } else {
                remaining
            };
            match self.rx.recv_timeout(wait) {
                Ok(Some(bytes)) => self.push(&bytes),
                Ok(None) => self.eof = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Poll timeouts inside the patience window are not command timeouts; loop and re-evaluate.
                    if self.partial_banner_since.is_some() && Instant::now() < deadline {
                        continue;
                    }
                    return ReadOutcome::Timeout {
                        partial: self.take_text(),
                    };
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.eof = true,
            }
        }
    }

    /// Append raw bytes and re-normalize/decode. Buffers are small, so repeated decoding is acceptable.
    /// New bytes reset partial-banner patience because a full banner may be continuing.
    fn push(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        let decoded = String::from_utf8_lossy(&self.raw);
        self.text = decoded.replace("\r\n", "\n").replace('\r', "\n");
        self.partial_banner_since = None;
    }

    /// Take and clear the current text buffer.
    fn take_text(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.raw.clear();
        out
    }

    /// Drain residual bytes already in or about to arrive through the channel, then clear the internal buffer.
    /// Used after blocking commands to remove tail output such as bare prompts or source lines.
    /// Loops with recv_timeout(100ms) to ensure trailing bytes being flushed by jdb are collected before discarding.
    pub fn drain_stale(&mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(bytes)) => self.push(&bytes),
                _ => break,
            }
        }
        let _ = self.take_text();
    }

    /// Non-blockingly clear residual bytes already in the channel plus the internal buffer.
    /// Called when the buffer should be empty before issuing a new command in Suspended state: remove stale
    /// bare-prompt `> ` bytes that arrived late from the previous blocking command so they are not treated
    /// as an empty response to the current command.
    pub fn purge_pending(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            match chunk {
                Some(bytes) => self.push(&bytes),
                None => {
                    self.eof = true;
                    break;
                }
            }
        }
        let _ = self.take_text();
    }

    /// Check whether the current buffer has reached a termination condition.
    fn try_match(&mut self, mode: ReadMode) -> Option<ReadOutcome> {
        // 1. Fatal errors take priority.
        if let Some(m) = RE_FATAL.find(&self.text) {
            let line = current_line(&self.text, m.start()).to_string();
            let _ = self.take_text();
            return Some(ReadOutcome::Fatal { message: line });
        }

        // 2. VM exit: the banner is enough; EOF may follow, and there may be no prompt.
        if RE_VM_EXIT.is_match(&self.text) {
            return Some(ReadOutcome::VmExit {
                output: self.take_text(),
            });
        }

        // 3. Tail prompt: decide whether the command is complete.
        let last_line = self.text.rsplit('\n').next().unwrap_or("");
        let is_thread_prompt = RE_THREAD_PROMPT.is_match(last_line);
        let is_bare_prompt = RE_BARE_PROMPT.is_match(last_line);

        // Thread prompts always mean stopped; bare prompts only complete Normal mode.
        let done = is_thread_prompt || (is_bare_prompt && mode == ReadMode::Normal);
        if done {
            // Thread prompts like `main[1] ` carry the current thread name, used to backfill event banners
            // without thread=, such as exceptions.
            let prompt_thread = is_thread_prompt
                .then(|| thread_from_prompt(last_line))
                .flatten();
            let cut = self.text.len() - last_line.len();
            let output = self.text[..cut].trim_end_matches('\n').to_string();
            let event = detect_event(&output, prompt_thread.as_deref());
            let _ = self.take_text();
            return Some(ReadOutcome::Prompt { output, event });
        }

        // Bare prompt in Blocking mode is usually ignored while waiting for a real thread-prompt/event banner.
        if is_bare_prompt && mode == ReadMode::Blocking {
            let before_prompt = &self.text[..self.text.len() - last_line.len()];

            // SUSPEND_THREAD policy (JDK 9+, observed on macOS/JDK 21 with `stop thread at/in`): a
            // breakpoint or step fires with a COMPLETE banner, but jdb returns to a bare prompt `> `
            // instead of a thread prompt `worker[1] ` and leaves NO current thread selected — any
            // following where/locals/print then fails with "No thread specified." Route this through
            // the PartialStop path so the session layer selects the hit thread and backfills
            // thread_id/frame/location via threads→thread<id>→where (same machinery as the JDK 8
            // truncated-banner case).
            if let Some(c) = RE_BREAKPOINT_OR_STEP.captures(before_prompt) {
                let is_step = &c["kind"] == "Step completed";
                let output = before_prompt.trim_end_matches('\n').to_string();
                let _ = self.take_text();
                return Some(ReadOutcome::Prompt {
                    output,
                    event: Some(DetectedEvent::PartialStop { is_step }),
                });
            }

            // Special case: "Nothing suspended." + bare prompt means the VM was not actually suspended,
            // e.g. cont after attach suspend=n. cont/resume is then a no-op and should return immediately
            // instead of timing out.
            if before_prompt.contains("Nothing suspended") {
                let output = before_prompt.trim_end_matches('\n').to_string();
                let _ = self.take_text();
                return Some(ReadOutcome::Prompt {
                    output,
                    event: None,
                });
            }
        }

        None
    }

    /// Whether the buffer tail is a truncated event banner prefix (`"Breakpoint hit: "` or `"Step completed: "`).
    /// Under JDK 8 SUSPEND_THREAD policy, after hitting a breakpoint or completing a step, jdb writes only
    /// this prefix and does not continue with thread/location or thread-prompt; the cursor stops after colon-space.
    fn tail_is_partial_banner(&self) -> bool {
        let last_line = self.text.rsplit('\n').next().unwrap_or("");
        last_line == "Breakpoint hit: " || last_line == "Step completed: "
    }

    /// Patience window expired, confirming a truncated banner; emit a PartialStop event.
    fn emit_partial_stop(&mut self) -> ReadOutcome {
        let last_line = self.text.rsplit('\n').next().unwrap_or("");
        let is_step = last_line.starts_with("Step completed:");
        let cut = self.text.len() - last_line.len();
        let output = self.text[..cut].trim_end_matches('\n').to_string();
        let _ = self.take_text();
        self.partial_banner_since = None;
        ReadOutcome::Prompt {
            output,
            event: Some(DetectedEvent::PartialStop { is_step }),
        }
    }
}

/// Detect event banners in `output` text.
/// `prompt_thread`: thread name inferred from the tail thread-prompt, used to backfill exception banners without thread=.
fn detect_event(output: &str, prompt_thread: Option<&str>) -> Option<DetectedEvent> {
    if let Some(c) = RE_BREAKPOINT_OR_STEP.captures(output) {
        let thread = c["thread"].to_string();
        let class = c["class"].to_string();
        let method = c["method"].to_string();
        let line = parse_line_number(&c["line"]);
        let is_step = &c["kind"] == "Step completed";
        return Some(if is_step {
            DetectedEvent::Step {
                thread,
                class,
                method,
                line,
            }
        } else {
            DetectedEvent::Breakpoint {
                thread,
                class,
                method,
                line,
            }
        });
    }
    if let Some(c) = RE_EXCEPTION.captures(output) {
        // Exception banners do not contain thread=; infer the current thread from the tail thread-prompt or leave empty.
        return Some(DetectedEvent::Exception {
            thread: prompt_thread.unwrap_or("").to_string(),
            exception: c["exc"].to_string(),
            caught: &c["caught"] == "caught",
        });
    }
    if let Some(c) = RE_FIELD_WATCH.captures(output) {
        let detail = &c["detail"];
        let access_type = if detail.contains("will be") {
            "modified".to_string()
        } else {
            "accessed".to_string()
        };
        return Some(DetectedEvent::FieldWatch {
            thread: c["thread"].to_string(),
            field: c["field"].to_string(),
            access_type,
            class: c["class"].to_string(),
            method: c["method"].to_string(),
            line: parse_line_number(&c["line"]),
        });
    }
    None
}

/// Parse a line number, stripping en_US thousands separators emitted by jdb, e.g. "3,956" → 3956.
fn parse_line_number(raw: &str) -> u32 {
    raw.replace(',', "").parse().unwrap_or(0)
}

/// Extract the thread name from a thread-prompt line such as `main[1] `, taking the part before `[`.
fn thread_from_prompt(line: &str) -> Option<String> {
    if !RE_THREAD_PROMPT.is_match(line) {
        return None;
    }
    line.split('[')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Return the whole line in `text` that contains byte offset `pos`.
fn current_line(text: &str, pos: usize) -> &str {
    let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[pos..]
        .find('\n')
        .map(|i| pos + i)
        .unwrap_or(text.len());
    &text[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_field_watch_modification() {
        // Real jdb format: Field (Class.field) is <old>, will be <new>: "thread=T", Class.method(), line=N bci=M
        let output = r#"Field (WatchTest.name) is null, will be "initial": "thread=main", WatchTest.<clinit>(), line=6 bci=2"#;
        let event = detect_event(output, Some("main"));
        let Some(DetectedEvent::FieldWatch {
            thread,
            field,
            access_type,
            class,
            method,
            line,
        }) = event
        else {
            panic!("expected FieldWatch, got {event:?}");
        };
        assert_eq!(thread, "main");
        assert_eq!(field, "WatchTest.name");
        assert_eq!(access_type, "modified");
        assert_eq!(class, "WatchTest");
        assert_eq!(method, "<clinit>");
        assert_eq!(line, 6);
    }

    #[test]
    fn detect_field_watch_access() {
        // Access format: no "will be" in the detail
        let output = r#"Field (com.example.Service.name) is "hello": "thread=worker-1", com.example.Service.getName(), line=15 bci=0"#;
        let event = detect_event(output, Some("worker-1"));
        let Some(DetectedEvent::FieldWatch {
            thread,
            field,
            access_type,
            class,
            method,
            line,
        }) = event
        else {
            panic!("expected FieldWatch, got {event:?}");
        };
        assert_eq!(thread, "worker-1");
        assert_eq!(field, "com.example.Service.name");
        assert_eq!(access_type, "accessed");
        assert_eq!(class, "com.example.Service");
        assert_eq!(method, "getName");
        assert_eq!(line, 15);
    }

    #[test]
    fn detect_breakpoint_takes_priority_over_field_watch() {
        let output = "Breakpoint hit: \"thread=main\", Main.main(), line=9 bci=0";
        let event = detect_event(output, Some("main"));
        assert!(matches!(event, Some(DetectedEvent::Breakpoint { .. })));
    }

    /// Regression: in en_US locale, jdb prints thousands separators for line numbers >=1000, e.g. `line=3,956`.
    #[test]
    fn breakpoint_line_with_thousands_separator() {
        let output = r#"Breakpoint hit: "thread=http-nio-8231-exec-3", com.yao.shopping.business.impl.cart.ShoppingCartListManagerImpl.getAllCartAndDemandDataV2(), line=3,956 bci=538"#;
        let event = detect_event(output, None);
        let Some(DetectedEvent::Breakpoint {
            thread,
            class,
            method,
            line,
        }) = event
        else {
            panic!("expected Breakpoint, got {event:?}");
        };
        assert_eq!(thread, "http-nio-8231-exec-3");
        assert_eq!(
            class,
            "com.yao.shopping.business.impl.cart.ShoppingCartListManagerImpl"
        );
        assert_eq!(method, "getAllCartAndDemandDataV2");
        assert_eq!(line, 3956, "thousands separator comma must be stripped");
    }

    /// Regression: ordinary line numbers without thousands separators still parse normally.
    #[test]
    fn breakpoint_line_without_separator_still_works() {
        let output = r#"Breakpoint hit: "thread=main", Main.main(), line=42 bci=0"#;
        let event = detect_event(output, None);
        let Some(DetectedEvent::Breakpoint { line, .. }) = event else {
            panic!("expected Breakpoint, got {event:?}");
        };
        assert_eq!(line, 42);
    }

    /// Field watchpoints may also have line numbers with thousands separators.
    #[test]
    fn field_watch_line_with_thousands_separator() {
        let output = r#"Field (Service.count) is 0, will be 1: "thread=worker-1", com.example.Service.increment(), line=1,024 bci=5"#;
        let event = detect_event(output, None);
        let Some(DetectedEvent::FieldWatch { line, .. }) = event else {
            panic!("expected FieldWatch, got {event:?}");
        };
        assert_eq!(line, 1024, "thousands separator comma must be stripped");
    }

    /// Reader that emits one preset byte chunk and then blocks forever without EOF.
    /// Simulates JDK 8 SUSPEND_THREAD where jdb writes a truncated prefix and then neither writes nor closes the pipe.
    struct StallingReader {
        data: Vec<u8>,
        sent: bool,
    }

    impl std::io::Read for StallingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let n = self.data.len().min(buf.len());
                buf[..n].copy_from_slice(&self.data[..n]);
                return Ok(n);
            }
            // Block forever afterwards: jdb stopped at a truncated banner, with no further output and no EOF.
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    #[test]
    fn partial_breakpoint_banner_emits_partial_stop_in_blocking_mode() {
        // JDK 8 SUSPEND_THREAD truncated banner: normal startup output first, and the final line is
        // `Breakpoint hit: ` ending with colon-space, no newline, no continuation.
        let truncated =
            b"VM Started: Set deferred breakpoint ThreadTest.doWork\n\nBreakpoint hit: ";
        let reader = StallingReader {
            data: truncated.to_vec(),
            sent: false,
        };
        let mut pr = PromptReader::new(reader);

        // Blocking mode: after the 500ms patience window, classify as PartialStop instead of waiting for timeout.
        let start = Instant::now();
        let outcome = pr.read_until_prompt(Duration::from_secs(30), ReadMode::Blocking);
        let elapsed = start.elapsed();

        match outcome {
            ReadOutcome::Prompt {
                event: Some(DetectedEvent::PartialStop { is_step }),
                output,
            } => {
                assert!(!is_step, "should be a breakpoint, not a step");
                assert!(
                    output.contains("VM Started"),
                    "output should retain pre-banner text, got: {output:?}"
                );
                assert!(
                    !output.contains("Breakpoint hit:"),
                    "the truncated banner line itself should be cut off, got: {output:?}"
                );
            }
            other => panic!("expected PartialStop, got {other:?}"),
        }
        // Should return shortly after the patience window, far below the 30s timeout.
        assert!(
            elapsed < Duration::from_secs(5),
            "should resolve via patience window (~500ms), not block until timeout; took {elapsed:?}"
        );
    }

    #[test]
    fn partial_step_banner_emits_partial_stop() {
        let truncated = b"Step completed: ";
        let reader = StallingReader {
            data: truncated.to_vec(),
            sent: false,
        };
        let mut pr = PromptReader::new(reader);

        let outcome = pr.read_until_prompt(Duration::from_secs(30), ReadMode::Blocking);
        match outcome {
            ReadOutcome::Prompt {
                event: Some(DetectedEvent::PartialStop { is_step }),
                ..
            } => {
                assert!(is_step, "should be flagged as a step");
            }
            other => panic!("expected PartialStop step, got {other:?}"),
        }
    }

    /// Bug A regression: internal buffers must be cleared after Timeout or later commands read stale data.
    #[test]
    fn timeout_clears_buffer() {
        // Simulate data without a prompt, then stall to trigger timeout.
        let data = b"some partial output without prompt";
        let reader = StallingReader {
            data: data.to_vec(),
            sent: false,
        };
        let mut pr = PromptReader::new(reader);

        // Use a very short timeout to trigger Timeout.
        let outcome = pr.read_until_prompt(Duration::from_millis(200), ReadMode::Normal);
        match &outcome {
            ReadOutcome::Timeout { partial } => {
                assert!(partial.contains("some partial output"), "got: {partial:?}");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }

        // Key assertion: internal buffers were cleared, both text and raw.
        assert!(
            pr.text.is_empty(),
            "text buffer should be empty after timeout, got: {:?}",
            pr.text
        );
        assert!(
            pr.raw.is_empty(),
            "raw buffer should be empty after timeout, got len={}",
            pr.raw.len()
        );
    }

    /// macOS/JDK 21 regression: SUSPEND_THREAD breakpoint produces a COMPLETE banner followed by a bare
    /// prompt `> ` (not a thread prompt). The reader should route this through PartialStop so the session
    /// layer selects the hit thread and enriches thread_id/frame/location.
    #[test]
    fn suspend_thread_full_banner_bare_prompt_emits_partial_stop() {
        // Real output observed on macOS JDK 21 with `stop thread in ThreadTest.doWork`:
        let data = b"VM Started: Set deferred breakpoint ThreadTest.doWork\n\nBreakpoint hit: \"thread=worker\", ThreadTest.doWork(), line=38 bci=0\n38            int x = 1;\n> ";
        let reader = StallingReader {
            data: data.to_vec(),
            sent: false,
        };
        let mut pr = PromptReader::new(reader);

        let start = Instant::now();
        let outcome = pr.read_until_prompt(Duration::from_secs(30), ReadMode::Blocking);
        let elapsed = start.elapsed();

        match outcome {
            ReadOutcome::Prompt {
                event: Some(DetectedEvent::PartialStop { is_step }),
                output,
            } => {
                assert!(!is_step, "should be a breakpoint, not a step");
                assert!(
                    output.contains("Breakpoint hit"),
                    "output should retain the banner text, got: {output:?}"
                );
            }
            other => panic!("expected PartialStop from full banner + bare prompt, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "should resolve immediately, not wait for timeout; took {elapsed:?}"
        );
    }
}
