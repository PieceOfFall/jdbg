//! clap 命令行定义——1:1 映射 CLAUDE.md §7 CLI 命令面。

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

    /// Register (or remove) the jdbg MCP server in Claude Code's config.
    Setup {
        /// Remove the registration instead of adding it.
        #[arg(long)]
        remove: bool,
        /// Print the config snippet instead of writing any files.
        #[arg(long)]
        print: bool,
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
    /// List threads.
    Threads,
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
