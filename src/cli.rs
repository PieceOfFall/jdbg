//! clap command-line definitions, mapping 1:1 to the CLI surface in CLAUDE.md §7.

use clap::{Parser, Subcommand};

/// jdbg — Java debugger CLI for AI agents.
#[derive(Parser, Debug)]
#[command(name = "jdbg", version, about = "Java debugger CLI powered by jdb")]
pub struct Cli {
    /// Target session ID (omit for default if exactly one exists).
    #[arg(long, global = true)]
    pub session: Option<String>,

    /// Output JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Timeout in seconds for jdb commands.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,

    /// Override path to jdb executable.
    #[arg(long, global = true)]
    pub jdb_path: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    // ── Session lifecycle ──
    /// Launch a Java program under jdb.
    Launch {
        /// Fully qualified main class name.
        main_class: String,
        /// Classpath entries (semicolon-separated on Windows).
        #[arg(long)]
        classpath: Option<String>,
        /// Source path entries.
        #[arg(long)]
        sourcepath: Option<String>,
        /// Application arguments (after --).
        #[arg(last = true)]
        app_args: Vec<String>,
        /// Extra jdb arguments (repeatable).
        #[arg(long = "jdb-arg")]
        jdb_args: Vec<String>,
        /// Session display name.
        #[arg(long)]
        name: Option<String>,
    },

    /// Attach to a running JVM via JDWP.
    Attach {
        /// Target host.
        #[arg(long, default_value = "localhost")]
        host: String,
        /// Target JDWP port.
        #[arg(long, default_value_t = 5005)]
        port: u16,
        /// Source path entries.
        #[arg(long)]
        sourcepath: Option<String>,
        /// Session display name.
        #[arg(long)]
        name: Option<String>,
    },

    /// Show session status.
    Status,

    /// List all sessions.
    List,

    /// Kill a debug session (sends quit to jdb).
    Kill,

    // ── Daemon control ──
    /// Daemon management subcommands.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Register (or remove) the jdbg MCP server for coding agents.
    Setup {
        /// Remove the registration instead of adding it.
        #[arg(long)]
        remove: bool,
        /// Print the config snippet instead of writing any files.
        #[arg(long)]
        print: bool,
        /// Agent targets: claude,codex,opencode,pi or auto/all/none.
        #[arg(long)]
        target: Option<String>,
        /// Use non-interactive defaults.
        #[arg(long)]
        yes: bool,
    },

    /// Update jdbg: remove old setup, install latest release from GitHub, then re-register.
    Update,

    // ── Breakpoints ──
    /// Set a line breakpoint: stop at Class:line.
    BreakAt {
        /// Class name (e.g. com.example.Main).
        class: String,
        /// Line number.
        line: u32,
        /// Conditional expression — breakpoint only fires when true.
        #[arg(long, short = 'c')]
        condition: Option<String>,
        /// Suspend policy: "all" (default) or "thread" (only suspend hit thread).
        #[arg(long, short = 's')]
        suspend: Option<String>,
    },

    /// Set a method breakpoint: stop in Class.method.
    BreakIn {
        /// Class name.
        class: String,
        /// Method name.
        method: String,
        /// Parameter types for overload disambiguation.
        #[arg(long)]
        args: Option<String>,
        /// Conditional expression — breakpoint only fires when true.
        #[arg(long, short = 'c')]
        condition: Option<String>,
        /// Suspend policy: "all" (default) or "thread" (only suspend hit thread).
        #[arg(long, short = 's')]
        suspend: Option<String>,
    },

    /// Catch an exception.
    Catch {
        /// Exception class name.
        exception: String,
        /// Mode: caught, uncaught, or all.
        #[arg(long, default_value = "all")]
        mode: String,
    },

    /// List current breakpoints.
    Breakpoints,

    /// Set a field watchpoint (break on access/modification).
    Watch {
        /// Field spec: Class.field (e.g. com.example.Service.name).
        field: String,
        /// Watch mode: access, modification (default), or all.
        #[arg(long, default_value = "modification")]
        mode: String,
    },

    /// Remove a field watchpoint.
    Unwatch {
        /// Field spec: Class.field.
        field: String,
        /// Watch mode to remove: access, modification (default), or all. Must match the mode used when setting.
        #[arg(long, default_value = "modification")]
        mode: String,
    },

    /// Clear a breakpoint (Class:line or Class.method).
    Clear {
        /// Breakpoint spec to clear.
        spec: String,
    },

    // ── Execution control ──
    /// Start the debugged application (launch mode only).
    Run,
    /// Continue execution.
    Cont,
    /// Step into.
    Step,
    /// Step over.
    Next,
    /// Step out (run until current method returns).
    StepOut,

    // ── Class/method search ──
    /// Search loaded classes (filter by substring pattern).
    Classes {
        /// Substring pattern to filter class names.
        pattern: Option<String>,
    },

    /// List all methods of a loaded class.
    Methods {
        /// Fully-qualified class name.
        class: String,
    },

    // ── Inspection ──
    /// Print call stack.
    Where {
        /// Show all threads.
        #[arg(long)]
        all: bool,
    },
    /// Print local variables.
    Locals,
    /// Print/evaluate an expression.
    Print {
        /// Expression to evaluate.
        expr: String,
    },
    /// Dump all fields of an object.
    Dump {
        /// Object expression.
        expr: String,
    },
    /// Evaluate an expression (alias for print).
    Eval {
        /// Expression.
        expr: String,
    },
    /// List threads (optionally filter by name substring).
    Threads {
        /// Case-insensitive substring to filter thread names (e.g. "http-nio").
        #[arg(long)]
        filter: Option<String>,
    },
    /// Switch to a thread by ID.
    Thread {
        /// Thread ID (hex from `threads` output).
        id: String,
    },
    /// Navigate stack frames.
    Frame {
        /// Direction: up or down.
        direction: String,
        /// Number of frames to move.
        #[arg(default_value_t = 1)]
        n: u32,
    },
    /// Show source code around current position.
    ListSource {
        /// Center on this line number.
        line: Option<u32>,
    },
    /// Inspect a collection/array: show size and first N elements.
    Inspect {
        /// Collection expression.
        expr: String,
        /// Max elements to show (default 10, max 50).
        #[arg(long, default_value_t = 10)]
        max_elements: u32,
    },
    /// Send a raw jdb command (escape hatch).
    Raw {
        /// The jdb command string.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    // ── Thread control / state mutation / locks ──
    /// Suspend a thread (or all threads if no id given).
    Suspend {
        /// Thread id (from `threads` output). Omit to suspend all threads.
        id: Option<String>,
    },
    /// Resume a thread (or all threads if no id given).
    Resume {
        /// Thread id (from `threads` output). Omit to resume all threads.
        id: Option<String>,
    },
    /// Assign a value to a variable, field, or array element.
    Set {
        /// Left-hand side: local var, field, or array element (e.g. "x", "this.count", "arr[0]").
        lvalue: String,
        /// Right-hand side expression (e.g. "42", "\"hello\"", "null").
        value: String,
    },
    /// Stop catching an exception (removes a `catch` breakpoint).
    Ignore {
        /// Exception class name or pattern.
        exception: String,
        /// Mode: caught, uncaught, or all (must match how it was caught).
        #[arg(long, default_value = "all")]
        mode: String,
    },
    /// Show monitor/lock info for an object.
    Lock {
        /// Object expression.
        expr: String,
    },
    /// Show locks held and awaited by a thread.
    ThreadLocks {
        /// Thread id (from `threads` output). Omit for the current thread.
        id: Option<String>,
    },

    /// Hidden: run as daemon (auto-spawned by CLI).
    #[command(name = "__daemon", hide = true)]
    Daemon_,

    /// Hidden: run as MCP server over stdio (spawned by Claude Code).
    #[command(name = "__mcp", hide = true)]
    Mcp_,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    /// Start the daemon (usually auto-started).
    Start,
    /// Stop the daemon.
    Stop,
    /// Show daemon status.
    Status,
}
