//! `jdbg setup` registers or removes jdbg for supported coding agents.
//!
//! First-version targets:
//! - Claude Code: `~/.claude.json`, `~/.claude/settings.json`, `~/.claude/skills/jdbg/`
//! - Codex: `~/.codex/config.toml`, `~/.codex/skills/jdbg/`
//! - OpenCode: `~/.config/opencode/opencode.json`, `~/.config/opencode/skills/jdbg/`
//! - Pi: `~/.pi/agent/skills/jdbg/`

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::jdi::lifecycle::default_sidecar_jar_path;
use crate::update_sidecar;

const MCP_SERVER_KEY: &str = "jdbg";
const PERMISSION_ENTRY: &str = "mcp__jdbg__*";
const CODEX_TOML_HEADER: &str = "mcp_servers.jdbg";
const OPENCODE_SCHEMA: &str = "https://opencode.ai/config.json";

const MCP_SKILL_MD: &str = include_str!("../skills/jdbg/mcp/SKILL.md");
const CLI_SKILL_MD: &str = include_str!("../skills/jdbg/cli/SKILL.md");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupBackend {
    Jdb,
    Jdi,
}

impl SetupBackend {
    pub fn id(self) -> &'static str {
        match self {
            SetupBackend::Jdb => "jdb",
            SetupBackend::Jdi => "jdi",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            SetupBackend::Jdb => "JDB",
            SetupBackend::Jdi => "JDI",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SetupBackend::Jdb => "JDB - literal raw jdb command stream and jdb-only commands",
            SetupBackend::Jdi => "JDI - default structured launch/attach inspect/events backend",
        }
    }

    fn setup_block(self, mcp_skill: bool) -> String {
        match (self, mcp_skill) {
            (SetupBackend::Jdb, true) => concat!(
                "> Setup preference: Preferred backend: JDB. Pass ",
                r#""backend": "jdb""#,
                " on `launch`/`attach` when you need literal raw jdb stdin passthrough or jdb-only commands. Omit `backend` or pass ",
                r#""backend": "jdi""#,
                " for the default JDI backend."
            )
            .into(),
            (SetupBackend::Jdb, false) => concat!(
                "> Setup preference: Preferred backend: JDB. Pass ",
                "`--backend jdb` on `launch`/`attach` when you need literal raw jdb stdin passthrough or jdb-only commands. Omit `--backend` or pass ",
                "`--backend jdi` for the default JDI backend."
            )
            .into(),
            (SetupBackend::Jdi, true) => concat!(
                "> Setup preference: Preferred backend: JDI. Omit `backend` on `launch`/`attach` for the default JDI backend; ",
                "it falls back to jdb only when local JDI prerequisites are missing. Pass ",
                r#""backend": "jdb""#,
                " when you need literal raw jdb stdin passthrough or jdb-only commands."
            )
            .into(),
            (SetupBackend::Jdi, false) => concat!(
                "> Setup preference: Preferred backend: JDI. Omit `--backend` on `launch`/`attach` for the default JDI backend; ",
                "it falls back to jdb only when local JDI prerequisites are missing. Pass ",
                "`--backend jdb` when you need literal raw jdb stdin passthrough or jdb-only commands."
            )
            .into(),
        }
    }
}

const SETUP_BACKENDS: [SetupBackend; 2] = [SetupBackend::Jdb, SetupBackend::Jdi];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetId {
    Claude,
    Codex,
    Opencode,
    Pi,
}

impl TargetId {
    fn id(self) -> &'static str {
        match self {
            TargetId::Claude => "claude",
            TargetId::Codex => "codex",
            TargetId::Opencode => "opencode",
            TargetId::Pi => "pi",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            TargetId::Claude => "Claude Code",
            TargetId::Codex => "Codex",
            TargetId::Opencode => "OpenCode",
            TargetId::Pi => "Pi",
        }
    }
}

const ALL_TARGETS: [TargetId; 4] = [
    TargetId::Claude,
    TargetId::Codex,
    TargetId::Opencode,
    TargetId::Pi,
];

#[derive(Debug)]
struct Paths {
    home: PathBuf,
}

impl Paths {
    fn new() -> Result<Self> {
        Ok(Self { home: home_dir()? })
    }

    #[cfg(test)]
    fn for_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    fn claude_config(&self) -> PathBuf {
        self.home.join(".claude.json")
    }

    fn claude_settings(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }

    fn claude_skill_dir(&self) -> PathBuf {
        self.home.join(".claude").join("skills").join("jdbg")
    }

    fn codex_dir(&self) -> PathBuf {
        self.home.join(".codex")
    }

    fn codex_config(&self) -> PathBuf {
        self.codex_dir().join("config.toml")
    }

    fn codex_skill_dir(&self) -> PathBuf {
        self.codex_dir().join("skills").join("jdbg")
    }

    fn opencode_dir(&self) -> PathBuf {
        self.home.join(".config").join("opencode")
    }

    fn opencode_config(&self) -> PathBuf {
        self.opencode_dir().join("opencode.json")
    }

    fn opencode_skill_dir(&self) -> PathBuf {
        self.opencode_dir().join("skills").join("jdbg")
    }

    fn pi_dir(&self) -> PathBuf {
        self.home.join(".pi").join("agent")
    }

    fn pi_settings(&self) -> PathBuf {
        self.pi_dir().join("settings.json")
    }

    fn pi_skill_root(&self) -> PathBuf {
        self.pi_dir().join("skills")
    }

    fn pi_skill_dir(&self) -> PathBuf {
        self.pi_skill_root().join("jdbg")
    }

    fn agents_skill_root(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }
}

fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("cannot determine home directory")
}

pub fn claude_config_path() -> Result<PathBuf> {
    Ok(Paths::new()?.claude_config())
}

pub fn claude_settings_path() -> Result<PathBuf> {
    Ok(Paths::new()?.claude_settings())
}

pub fn skill_dir() -> Result<PathBuf> {
    Ok(Paths::new()?.claude_skill_dir())
}

fn mcp_server_value() -> Value {
    json!({
        "command": "jdbg",
        "args": ["__mcp"]
    })
}

fn opencode_mcp_server_value() -> Value {
    json!({
        "type": "local",
        "command": ["jdbg", "__mcp"],
        "enabled": true
    })
}

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn read_json_strict(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("failed to parse JSON config {}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)? + "\n";
    atomic_write(path, content.as_bytes())
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension().and_then(|s| s.to_str()).unwrap_or(""),
        std::process::id()
    ));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path).context("atomic rename failed")?;
    Ok(())
}

fn install_skill(dir: &Path, content: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    fs::write(&path, content)?;
    Ok(path)
}

fn render_skill(template: &str, backend: SetupBackend) -> String {
    let mcp_skill = template.contains("MCP server") || template.contains("mcp__jdbg__");
    let block = backend.setup_block(mcp_skill);
    let Some(heading_start) = template.find("\n# ") else {
        return format!("{}\n\n{}\n", template.trim_end(), block);
    };
    let heading_start = heading_start + 1;
    let insert_at = template[heading_start..]
        .find('\n')
        .map(|offset| heading_start + offset + 1)
        .unwrap_or(template.len());
    format!(
        "{}\n{}\n{}",
        &template[..insert_at],
        block,
        &template[insert_at..]
    )
}

fn remove_skill(dir: &Path) -> Result<bool> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn apply_mcp_install(config: &mut Value) {
    if !config.is_object() {
        *config = json!({});
    }
    let servers = config
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_KEY.to_owned(), mcp_server_value());
}

fn apply_mcp_remove(config: &mut Value) -> bool {
    let Some(obj) = config.as_object_mut() else {
        return false;
    };
    let Some(servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    if servers.remove(MCP_SERVER_KEY).is_none() {
        return false;
    }
    if servers.is_empty() {
        obj.remove("mcpServers");
    }
    true
}

fn apply_opencode_mcp_install(config: &mut Value) {
    if !config.is_object() {
        *config = json!({});
    }
    let obj = config.as_object_mut().unwrap();
    obj.entry("$schema")
        .or_insert_with(|| json!(OPENCODE_SCHEMA));
    let servers = obj.entry("mcp").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_KEY.to_owned(), opencode_mcp_server_value());
}

fn apply_opencode_mcp_remove(config: &mut Value) -> bool {
    let Some(obj) = config.as_object_mut() else {
        return false;
    };
    let Some(servers) = obj.get_mut("mcp").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    if servers.remove(MCP_SERVER_KEY).is_none() {
        return false;
    }
    if servers.is_empty() {
        obj.remove("mcp");
    }
    true
}

fn apply_perm_install(settings: &mut Value) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let perms = settings
        .as_object_mut()
        .unwrap()
        .entry("permissions")
        .or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let allow = perms
        .as_object_mut()
        .unwrap()
        .entry("allow")
        .or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let arr = allow.as_array_mut().unwrap();
    let entry = json!(PERMISSION_ENTRY);
    if !arr.contains(&entry) {
        arr.push(entry);
    }
}

fn apply_perm_remove(settings: &mut Value) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    let Some(perms) = obj.get_mut("permissions").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let Some(allow) = perms.get_mut("allow").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let entry = json!(PERMISSION_ENTRY);
    let before = allow.len();
    allow.retain(|v| v != &entry);
    if allow.len() == before {
        return false;
    }
    if allow.is_empty() {
        perms.remove("allow");
    }
    if perms.is_empty() {
        obj.remove("permissions");
    }
    true
}

fn quote_toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn codex_mcp_block() -> String {
    format!(
        "[{CODEX_TOML_HEADER}]\ncommand = {}\nargs = [{}]",
        quote_toml_string("jdbg"),
        quote_toml_string("__mcp")
    )
}

fn find_toml_header(content: &str, header: &str) -> Option<usize> {
    let header_line = format!("[{header}]");
    if content.starts_with(&header_line) {
        return Some(0);
    }
    content.find(&format!("\n{header_line}")).map(|idx| idx + 1)
}

fn find_next_toml_table(content: &str, from: usize) -> usize {
    let mut i = from;
    while i < content.len() {
        let Some(idx) = content[i..].find("\n[") else {
            return content.len();
        };
        let absolute = i + idx;
        if content.as_bytes().get(absolute + 2) == Some(&b'[') {
            i = absolute + 2;
            continue;
        }
        return absolute + 1;
    }
    content.len()
}

fn upsert_toml_table(content: &str, header: &str, block: &str) -> (String, bool) {
    let Some(start) = find_toml_header(content, header) else {
        let trimmed = content.trim_end();
        let sep = if trimmed.is_empty() { "" } else { "\n\n" };
        return (format!("{trimmed}{sep}{block}\n"), true);
    };

    let end = find_next_toml_table(content, start + header.len() + 2);
    let existing = content[start..end].trim_end_matches('\n');
    if existing == block {
        return (content.to_owned(), false);
    }

    let before = content[..start].trim_end_matches('\n');
    let after = content[end..].trim_start_matches('\n');
    let before_sep = if before.is_empty() { "" } else { "\n\n" };
    let after_sep = if after.is_empty() { "\n" } else { "\n\n" };
    (
        format!("{before}{before_sep}{block}{after_sep}{after}"),
        true,
    )
}

fn remove_toml_table(content: &str, header: &str) -> (String, bool) {
    let Some(start) = find_toml_header(content, header) else {
        return (content.to_owned(), false);
    };
    let end = find_next_toml_table(content, start + header.len() + 2);
    let before = content[..start].trim_end_matches('\n');
    let after = content[end..].trim_start_matches('\n');
    let sep = if before.is_empty() || after.is_empty() {
        ""
    } else {
        "\n\n"
    };
    (format!("{before}{sep}{after}"), true)
}

fn detect_claude_configured(paths: &Paths) -> bool {
    let config = read_json(&paths.claude_config());
    config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .and_then(|servers| servers.get(MCP_SERVER_KEY))
        .is_some()
}

fn detect_codex_configured(paths: &Paths) -> bool {
    fs::read_to_string(paths.codex_config())
        .map(|content| find_toml_header(&content, CODEX_TOML_HEADER).is_some())
        .unwrap_or(false)
}

fn detect_opencode_configured(paths: &Paths) -> bool {
    read_json_strict(&paths.opencode_config())
        .ok()
        .and_then(|config| {
            config
                .get("mcp")
                .and_then(|v| v.as_object())
                .and_then(|servers| servers.get(MCP_SERVER_KEY))
                .cloned()
        })
        .is_some()
}

fn detect_pi_configured(paths: &Paths) -> bool {
    paths.pi_skill_dir().join("SKILL.md").exists()
}

fn detect_installed(target: TargetId, paths: &Paths) -> bool {
    match target {
        TargetId::Claude => paths.home.join(".claude").exists() || paths.claude_config().exists(),
        TargetId::Codex => paths.codex_dir().exists() || paths.codex_config().exists(),
        TargetId::Opencode => paths.opencode_dir().exists() || paths.opencode_config().exists(),
        TargetId::Pi => {
            paths.pi_dir().exists()
                || paths.pi_settings().exists()
                || paths.pi_skill_root().exists()
                || paths.agents_skill_root().exists()
        }
    }
}

fn detect_configured(target: TargetId, paths: &Paths) -> bool {
    match target {
        TargetId::Claude => detect_claude_configured(paths),
        TargetId::Codex => detect_codex_configured(paths),
        TargetId::Opencode => detect_opencode_configured(paths),
        TargetId::Pi => detect_pi_configured(paths),
    }
}

fn detect_configured_targets(paths: &Paths) -> Vec<TargetId> {
    ALL_TARGETS
        .iter()
        .copied()
        .filter(|target| detect_configured(*target, paths))
        .collect()
}

fn detect_backend_preference(paths: &Paths) -> Option<SetupBackend> {
    [
        paths.claude_skill_dir().join("SKILL.md"),
        paths.codex_skill_dir().join("SKILL.md"),
        paths.opencode_skill_dir().join("SKILL.md"),
        paths.pi_skill_dir().join("SKILL.md"),
    ]
    .iter()
    .filter_map(|path| fs::read_to_string(path).ok())
    .find_map(|content| {
        if content.contains("Preferred backend: JDI") {
            Some(SetupBackend::Jdi)
        } else if content.contains("Preferred backend: JDB") {
            Some(SetupBackend::Jdb)
        } else {
            None
        }
    })
}

pub fn configured_targets_or_default() -> Result<Vec<TargetId>> {
    let paths = Paths::new()?;
    let configured = detect_configured_targets(&paths);
    if configured.is_empty() {
        Ok(vec![TargetId::Claude])
    } else {
        Ok(configured)
    }
}

pub fn configured_backend_or_default() -> Result<SetupBackend> {
    let paths = Paths::new()?;
    Ok(detect_backend_preference(&paths).unwrap_or(SetupBackend::Jdi))
}

pub fn targets_to_arg(targets: &[TargetId]) -> String {
    targets
        .iter()
        .map(|target| target.id())
        .collect::<Vec<_>>()
        .join(",")
}

fn resolve_auto_targets(paths: &Paths) -> Vec<TargetId> {
    let configured = detect_configured_targets(paths);
    if !configured.is_empty() {
        return configured;
    }
    let installed = ALL_TARGETS
        .iter()
        .copied()
        .filter(|target| detect_installed(*target, paths))
        .collect::<Vec<_>>();
    if !installed.is_empty() {
        return installed;
    }
    vec![TargetId::Claude]
}

fn target_by_id(id: &str) -> Option<TargetId> {
    match id {
        "claude" => Some(TargetId::Claude),
        "codex" => Some(TargetId::Codex),
        "opencode" => Some(TargetId::Opencode),
        "pi" => Some(TargetId::Pi),
        _ => None,
    }
}

fn parse_target_flag(value: &str, paths: &Paths) -> Result<Vec<TargetId>> {
    match value {
        "auto" => return Ok(resolve_auto_targets(paths)),
        "all" => return Ok(ALL_TARGETS.to_vec()),
        "none" => return Ok(Vec::new()),
        _ => {}
    }

    let mut targets = Vec::new();
    let mut unknown = Vec::new();
    for raw in value.split(',') {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        match target_by_id(id) {
            Some(target) if !targets.contains(&target) => targets.push(target),
            Some(_) => {}
            None => unknown.push(id.to_owned()),
        }
    }

    if !unknown.is_empty() {
        bail!(
            "unknown --target id(s): {}. Known: claude, codex, opencode, pi, plus auto/all/none",
            unknown.join(", ")
        );
    }
    Ok(targets)
}

fn setup_backend_by_id(id: &str) -> Option<SetupBackend> {
    match id {
        "jdb" => Some(SetupBackend::Jdb),
        "jdi" => Some(SetupBackend::Jdi),
        _ => None,
    }
}

fn parse_setup_backend_flag(value: &str) -> Result<SetupBackend> {
    let id = value.trim().to_ascii_lowercase();
    setup_backend_by_id(&id)
        .ok_or_else(|| anyhow::anyhow!("unknown --backend '{value}'. Known: jdb, jdi"))
}

fn prompt_backend(default: SetupBackend) -> Result<SetupBackend> {
    let labels: Vec<String> = SETUP_BACKENDS
        .iter()
        .map(|backend| backend.label().to_string())
        .collect();
    let default_idx = SETUP_BACKENDS
        .iter()
        .position(|backend| *backend == default)
        .unwrap_or(0);

    match crate::tui::single_select(
        "Which backend should installed skills prefer?",
        &labels,
        default_idx,
    ) {
        Ok(Some(idx)) => Ok(SETUP_BACKENDS[idx]),
        Ok(None) => bail!("setup cancelled"),
        Err(_) => prompt_backend_text(default),
    }
}

fn prompt_backend_text(default: SetupBackend) -> Result<SetupBackend> {
    println!("Which backend should installed skills prefer?");
    for (idx, backend) in SETUP_BACKENDS.iter().enumerate() {
        let checked = if *backend == default { "\u{2713}" } else { " " };
        println!("  {}. ({}) {}", idx + 1, checked, backend.label());
    }
    print!(
        "Select backend by number or id [default: {}]: ",
        default.id()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        return Ok(default);
    }
    match input {
        "1" => Ok(SetupBackend::Jdb),
        "2" => Ok(SetupBackend::Jdi),
        other => parse_setup_backend_flag(other),
    }
}

fn prompt_targets(defaults: &[TargetId]) -> Result<Vec<TargetId>> {
    let labels: Vec<String> = ALL_TARGETS
        .iter()
        .map(|t| format!("{} ({})", t.display_name(), t.id()))
        .collect();
    let initial: Vec<bool> = ALL_TARGETS.iter().map(|t| defaults.contains(t)).collect();

    match crate::tui::multi_select("Which agents should jdbg configure?", &labels, &initial) {
        Ok(Some(states)) => Ok(ALL_TARGETS
            .iter()
            .zip(states)
            .filter_map(|(t, on)| on.then_some(*t))
            .collect()),
        Ok(None) => bail!("setup cancelled"),
        // Raw terminal mode unavailable (e.g. a dumb terminal): fall back to the
        // plain line-based prompt.
        Err(_) => prompt_targets_text(defaults),
    }
}

fn prompt_targets_text(defaults: &[TargetId]) -> Result<Vec<TargetId>> {
    println!("Which agents should jdbg configure?");
    for (idx, target) in ALL_TARGETS.iter().enumerate() {
        let checked = if defaults.contains(target) { "*" } else { " " };
        println!(
            "  {}. [{}] {} ({})",
            idx + 1,
            checked,
            target.display_name(),
            target.id()
        );
    }
    print!(
        "Select agents by number or id, comma-separated [default: {}]: ",
        targets_to_arg(defaults)
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        return Ok(defaults.to_vec());
    }

    let mut selected = Vec::new();
    let mut unknown = Vec::new();
    for raw in input.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let target = match item {
            "1" => Some(TargetId::Claude),
            "2" => Some(TargetId::Codex),
            "3" => Some(TargetId::Opencode),
            "4" => Some(TargetId::Pi),
            other => target_by_id(other),
        };
        match target {
            Some(target) if !selected.contains(&target) => selected.push(target),
            Some(_) => {}
            None => unknown.push(item.to_owned()),
        }
    }
    if !unknown.is_empty() {
        bail!("unknown selection(s): {}", unknown.join(", "));
    }
    Ok(selected)
}

fn select_targets(
    remove: bool,
    target: Option<&str>,
    yes: bool,
    paths: &Paths,
) -> Result<Vec<TargetId>> {
    if let Some(value) = target {
        return parse_target_flag(value, paths);
    }

    if remove {
        let configured = detect_configured_targets(paths);
        return if configured.is_empty() {
            Ok(vec![TargetId::Claude])
        } else {
            Ok(configured)
        };
    }

    let defaults = resolve_auto_targets(paths);
    if yes || !io::stdin().is_terminal() {
        return Ok(defaults);
    }
    prompt_targets(&defaults)
}

fn select_backend(
    remove: bool,
    print: bool,
    backend: Option<&str>,
    yes: bool,
    targets_empty: bool,
) -> Result<SetupBackend> {
    let default = SetupBackend::Jdi;
    if let Some(value) = backend {
        return parse_setup_backend_flag(value);
    }
    if remove || print || targets_empty || yes || !io::stdin().is_terminal() {
        return Ok(default);
    }
    prompt_backend(default)
}

fn ensure_jdi_sidecar_available_with<F>(
    backend: SetupBackend,
    expected: &Path,
    exe: &Path,
    install: F,
) -> Result<Option<PathBuf>>
where
    F: FnOnce(&Path) -> Result<PathBuf>,
{
    if backend != SetupBackend::Jdi {
        return Ok(None);
    }

    if expected.is_file() {
        return Ok(None);
    }

    let installed = install(exe).with_context(|| {
        format!(
            "JDI backend was selected, but {} is missing and the official sidecar jar could not be installed",
            expected.display()
        )
    })?;
    Ok(Some(installed))
}

fn ensure_jdi_sidecar_available(backend: SetupBackend) -> Result<Option<PathBuf>> {
    if backend != SetupBackend::Jdi {
        return Ok(None);
    }

    let expected =
        default_sidecar_jar_path().context("cannot determine default JDI sidecar path")?;
    let exe = std::env::current_exe().context("cannot determine current exe path")?;
    ensure_jdi_sidecar_available_with(
        backend,
        &expected,
        &exe,
        update_sidecar::install_from_latest_release_next_to,
    )
}

fn install_claude(paths: &Paths, backend: SetupBackend) -> Result<Vec<PathBuf>> {
    let config_path = paths.claude_config();
    let settings_path = paths.claude_settings();

    let mut config = read_json(&config_path);
    let mut settings = read_json(&settings_path);
    apply_mcp_install(&mut config);
    apply_perm_install(&mut settings);
    write_json(&config_path, &config)?;
    write_json(&settings_path, &settings)?;
    let skill = render_skill(MCP_SKILL_MD, backend);
    let skill_path = install_skill(&paths.claude_skill_dir(), &skill)?;

    Ok(vec![config_path, settings_path, skill_path])
}

fn remove_claude(paths: &Paths) -> Result<Vec<PathBuf>> {
    let config_path = paths.claude_config();
    let settings_path = paths.claude_settings();
    let mut changed = Vec::new();

    let mut config = read_json(&config_path);
    if apply_mcp_remove(&mut config) {
        write_json(&config_path, &config)?;
        changed.push(config_path);
    }

    let mut settings = read_json(&settings_path);
    if apply_perm_remove(&mut settings) {
        write_json(&settings_path, &settings)?;
        changed.push(settings_path);
    }

    if remove_skill(&paths.claude_skill_dir())? {
        changed.push(paths.claude_skill_dir());
    }

    Ok(changed)
}

fn install_codex(paths: &Paths, backend: SetupBackend) -> Result<Vec<PathBuf>> {
    let config_path = paths.codex_config();
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let (next, changed) = upsert_toml_table(&existing, CODEX_TOML_HEADER, &codex_mcp_block());
    if changed {
        atomic_write(&config_path, next.as_bytes())?;
    }
    let skill = render_skill(MCP_SKILL_MD, backend);
    let skill_path = install_skill(&paths.codex_skill_dir(), &skill)?;
    Ok(vec![config_path, skill_path])
}

fn remove_codex(paths: &Paths) -> Result<Vec<PathBuf>> {
    let config_path = paths.codex_config();
    let mut changed = Vec::new();
    if let Ok(existing) = fs::read_to_string(&config_path) {
        let (next, removed) = remove_toml_table(&existing, CODEX_TOML_HEADER);
        if removed {
            if next.trim().is_empty() {
                let _ = fs::remove_file(&config_path);
            } else {
                atomic_write(&config_path, format!("{}\n", next.trim_end()).as_bytes())?;
            }
            changed.push(config_path);
        }
    }
    if remove_skill(&paths.codex_skill_dir())? {
        changed.push(paths.codex_skill_dir());
    }
    Ok(changed)
}

fn install_opencode(paths: &Paths, backend: SetupBackend) -> Result<Vec<PathBuf>> {
    let config_path = paths.opencode_config();
    let mut config = read_json_strict(&config_path)?;
    apply_opencode_mcp_install(&mut config);
    write_json(&config_path, &config)?;
    let skill = render_skill(MCP_SKILL_MD, backend);
    let skill_path = install_skill(&paths.opencode_skill_dir(), &skill)?;
    Ok(vec![config_path, skill_path])
}

fn remove_opencode(paths: &Paths) -> Result<Vec<PathBuf>> {
    let config_path = paths.opencode_config();
    let mut changed = Vec::new();

    if config_path.exists() {
        let mut config = read_json_strict(&config_path)?;
        if apply_opencode_mcp_remove(&mut config) {
            if config.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                let _ = fs::remove_file(&config_path);
            } else {
                write_json(&config_path, &config)?;
            }
            changed.push(config_path);
        }
    }

    if remove_skill(&paths.opencode_skill_dir())? {
        changed.push(paths.opencode_skill_dir());
    }
    Ok(changed)
}

fn install_pi(paths: &Paths, backend: SetupBackend) -> Result<Vec<PathBuf>> {
    let skill = render_skill(CLI_SKILL_MD, backend);
    let skill_path = install_skill(&paths.pi_skill_dir(), &skill)?;
    Ok(vec![skill_path])
}

fn remove_pi(paths: &Paths) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    if remove_skill(&paths.pi_skill_dir())? {
        changed.push(paths.pi_skill_dir());
    }
    Ok(changed)
}

fn print_target_config(target: TargetId, paths: &Paths, backend: SetupBackend) -> Result<()> {
    match target {
        TargetId::Claude => {
            let snippet = json!({ "mcpServers": { MCP_SERVER_KEY: mcp_server_value() } });
            println!("# Claude Code: add to {}", paths.claude_config().display());
            println!("{}", serde_json::to_string_pretty(&snippet)?);
            println!("# Installed skill preference: backend={}", backend.id());
        }
        TargetId::Codex => {
            println!("# Codex: add to {}", paths.codex_config().display());
            println!("{}", codex_mcp_block());
            println!("# Installed skill preference: backend={}", backend.id());
        }
        TargetId::Opencode => {
            let snippet = json!({
                "$schema": OPENCODE_SCHEMA,
                "mcp": { MCP_SERVER_KEY: opencode_mcp_server_value() }
            });
            println!("# OpenCode: add to {}", paths.opencode_config().display());
            println!("{}", serde_json::to_string_pretty(&snippet)?);
            println!(
                "# OpenCode discovers skills under {}.",
                paths.opencode_dir().join("skills").display()
            );
            println!("# Installed skill preference: backend={}", backend.id());
        }
        TargetId::Pi => {
            println!(
                "# Pi: install the CLI skill at {}",
                paths.pi_skill_dir().join("SKILL.md").display()
            );
            println!("# Pi discovers skills under ~/.pi/agent/skills/ automatically.");
            println!("# No MCP config is emitted because Pi has no official jdbg MCP setup.");
            println!("# Installed skill preference: backend={}", backend.id());
        }
    }
    Ok(())
}

pub fn run_setup(
    remove: bool,
    print: bool,
    target: Option<&str>,
    yes: bool,
    backend: Option<&str>,
) -> Result<()> {
    let paths = Paths::new()?;
    let targets = if print {
        match target {
            Some(value) => parse_target_flag(value, &paths)?,
            None => vec![TargetId::Claude],
        }
    } else {
        select_targets(remove, target, yes, &paths)?
    };
    let backend = select_backend(remove, print, backend, yes, targets.is_empty())?;

    if print {
        for (idx, target) in targets.iter().enumerate() {
            if idx > 0 {
                println!();
            }
            print_target_config(*target, &paths, backend)?;
        }
        return Ok(());
    }

    if targets.is_empty() {
        println!("No agent targets selected; nothing to do.");
        return Ok(());
    }

    if remove {
        let mut removed_any = false;
        for target in targets {
            let changed = match target {
                TargetId::Claude => remove_claude(&paths)?,
                TargetId::Codex => remove_codex(&paths)?,
                TargetId::Opencode => remove_opencode(&paths)?,
                TargetId::Pi => remove_pi(&paths)?,
            };
            if changed.is_empty() {
                println!("{}: jdbg was not registered.", target.display_name());
            } else {
                removed_any = true;
                println!("Removed jdbg from {}.", target.display_name());
                for path in changed {
                    println!("  {}", path.display());
                }
            }
        }
        if !removed_any {
            println!("Nothing to remove.");
        }
    } else {
        if let Some(path) = ensure_jdi_sidecar_available(backend)? {
            println!("Installed JDI sidecar at {}.", path.display());
        }
        for target in targets {
            let written = match target {
                TargetId::Claude => install_claude(&paths, backend)?,
                TargetId::Codex => install_codex(&paths, backend)?,
                TargetId::Opencode => install_opencode(&paths, backend)?,
                TargetId::Pi => install_pi(&paths, backend)?,
            };
            println!("Registered jdbg for {}.", target.display_name());
            for path in written {
                println!("  {}", path.display());
            }
        }
        println!(
            "Preferred backend recorded in installed skills: {}.",
            backend.display_name()
        );
        println!("Restart or reload the configured agent(s) to pick up jdbg.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("jdbg-setup-{label}-{}-{id}", std::process::id()))
    }

    #[test]
    fn install_into_fresh_config() {
        let mut config = json!({});
        apply_mcp_install(&mut config);
        assert_eq!(config["mcpServers"][MCP_SERVER_KEY], mcp_server_value());
    }

    #[test]
    fn install_preserves_other_keys() {
        let mut config = json!({
            "mcpServers": { "other": { "command": "x" } },
            "numStartups": 42
        });
        apply_mcp_install(&mut config);
        assert_eq!(config["mcpServers"][MCP_SERVER_KEY], mcp_server_value());
        assert_eq!(config["mcpServers"]["other"]["command"], json!("x"));
        assert_eq!(config["numStartups"], json!(42));
    }

    #[test]
    fn install_is_idempotent() {
        let mut config = json!({});
        apply_mcp_install(&mut config);
        apply_mcp_install(&mut config);
        assert_eq!(config["mcpServers"][MCP_SERVER_KEY], mcp_server_value());
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn opencode_install_preserves_other_servers_and_schema() {
        let mut config = json!({
            "mcp": { "other": { "type": "local", "command": ["x"] } },
            "theme": "system"
        });
        apply_opencode_mcp_install(&mut config);
        assert_eq!(config["$schema"], json!(OPENCODE_SCHEMA));
        assert_eq!(config["mcp"][MCP_SERVER_KEY], opencode_mcp_server_value());
        assert_eq!(config["mcp"]["other"]["command"], json!(["x"]));
        assert_eq!(config["theme"], json!("system"));
    }

    #[test]
    fn opencode_remove_keeps_sibling_servers() {
        let mut config = json!({
            "mcp": {
                MCP_SERVER_KEY: opencode_mcp_server_value(),
                "other": { "type": "local", "command": ["x"] }
            }
        });
        let changed = apply_opencode_mcp_remove(&mut config);
        assert!(changed);
        assert!(config["mcp"].get(MCP_SERVER_KEY).is_none());
        assert_eq!(config["mcp"]["other"]["command"], json!(["x"]));
    }

    #[test]
    fn remove_existing_and_clean_empty() {
        let mut config = json!({ "mcpServers": { MCP_SERVER_KEY: mcp_server_value() } });
        let changed = apply_mcp_remove(&mut config);
        assert!(changed);
        assert!(config.get("mcpServers").is_none());
    }

    #[test]
    fn remove_keeps_other_servers() {
        let mut config = json!({
            "mcpServers": { MCP_SERVER_KEY: mcp_server_value(), "other": { "command": "x" } }
        });
        let changed = apply_mcp_remove(&mut config);
        assert!(changed);
        assert!(config["mcpServers"].get(MCP_SERVER_KEY).is_none());
        assert_eq!(config["mcpServers"]["other"]["command"], json!("x"));
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut config = json!({ "numStartups": 1 });
        let changed = apply_mcp_remove(&mut config);
        assert!(!changed);
        assert_eq!(config["numStartups"], json!(1));
    }

    #[test]
    fn perm_install_creates_allow_array() {
        let mut settings = json!({});
        apply_perm_install(&mut settings);
        assert_eq!(settings["permissions"]["allow"][0], json!(PERMISSION_ENTRY));
    }

    #[test]
    fn perm_install_appends_without_dup() {
        let mut settings = json!({ "permissions": { "allow": ["existing"] } });
        apply_perm_install(&mut settings);
        apply_perm_install(&mut settings);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow.len(), 2);
        assert!(allow.contains(&json!("existing")));
        assert!(allow.contains(&json!(PERMISSION_ENTRY)));
    }

    #[test]
    fn perm_remove_cleans_up() {
        let mut settings = json!({ "permissions": { "allow": [PERMISSION_ENTRY] } });
        let changed = apply_perm_remove(&mut settings);
        assert!(changed);
        assert!(settings.get("permissions").is_none());
    }

    #[test]
    fn perm_remove_keeps_other_entries() {
        let mut settings = json!({ "permissions": { "allow": [PERMISSION_ENTRY, "keep"] } });
        let changed = apply_perm_remove(&mut settings);
        assert!(changed);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow, &vec![json!("keep")]);
    }

    #[test]
    fn perm_remove_nonexistent_is_noop() {
        let mut settings = json!({ "other": true });
        let changed = apply_perm_remove(&mut settings);
        assert!(!changed);
        assert_eq!(settings["other"], json!(true));
    }

    #[test]
    fn parse_targets_handles_special_values_and_csv() {
        let paths = Paths::for_home(temp_home("parse"));
        assert_eq!(
            parse_target_flag("none", &paths).unwrap(),
            Vec::<TargetId>::new()
        );
        assert_eq!(
            parse_target_flag("all", &paths).unwrap(),
            ALL_TARGETS.to_vec()
        );
        assert_eq!(
            parse_target_flag("claude,codex,opencode,pi,claude", &paths).unwrap(),
            vec![
                TargetId::Claude,
                TargetId::Codex,
                TargetId::Opencode,
                TargetId::Pi
            ]
        );
        assert!(parse_target_flag("claude,bogus", &paths).is_err());
    }

    #[test]
    fn auto_targets_prefer_configured_then_installed_then_claude() {
        let home = temp_home("auto");
        let paths = Paths::for_home(&home);
        assert_eq!(resolve_auto_targets(&paths), vec![TargetId::Claude]);

        fs::create_dir_all(paths.codex_dir()).unwrap();
        assert_eq!(resolve_auto_targets(&paths), vec![TargetId::Codex]);

        let _ = fs::remove_dir_all(paths.codex_dir());
        fs::create_dir_all(paths.opencode_dir()).unwrap();
        assert_eq!(resolve_auto_targets(&paths), vec![TargetId::Opencode]);

        let _ = fs::remove_dir_all(paths.opencode_dir());
        fs::create_dir_all(paths.pi_dir()).unwrap();
        assert_eq!(resolve_auto_targets(&paths), vec![TargetId::Pi]);

        let _ = fs::remove_dir_all(paths.pi_dir());
        fs::create_dir_all(paths.claude_config().parent().unwrap()).unwrap();
        let mut config = json!({});
        apply_mcp_install(&mut config);
        write_json(&paths.claude_config(), &config).unwrap();
        assert_eq!(resolve_auto_targets(&paths), vec![TargetId::Claude]);

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn codex_toml_upsert_insert_replace_preserve_and_remove() {
        let block = codex_mcp_block();
        let (inserted, changed) = upsert_toml_table("", CODEX_TOML_HEADER, &block);
        assert!(changed);
        assert!(inserted.contains("[mcp_servers.jdbg]"));
        assert!(inserted.contains("command = \"jdbg\""));
        assert!(inserted.contains("args = [\"__mcp\"]"));

        let (same, changed) = upsert_toml_table(&inserted, CODEX_TOML_HEADER, &block);
        assert!(!changed);
        assert_eq!(same, inserted);

        let existing = "[other]\nfoo = \"bar\"\n\n[mcp_servers.jdbg]\ncommand = \"old\"\nargs = [\"x\"]\n\n[tail]\nvalue = \"ok\"\n";
        let (replaced, changed) = upsert_toml_table(existing, CODEX_TOML_HEADER, &block);
        assert!(changed);
        assert!(replaced.contains("[other]"));
        assert!(replaced.contains("[tail]"));
        assert!(!replaced.contains("old"));
        assert!(replaced.contains("command = \"jdbg\""));

        let (removed, did_remove) = remove_toml_table(&replaced, CODEX_TOML_HEADER);
        assert!(did_remove);
        assert!(removed.contains("[other]"));
        assert!(removed.contains("[tail]"));
        assert!(!removed.contains("[mcp_servers.jdbg]"));
    }

    #[test]
    fn detect_configured_finds_claude_codex_opencode_and_pi() {
        let home = temp_home("detect");
        let paths = Paths::for_home(&home);
        assert!(detect_configured_targets(&paths).is_empty());

        let mut config = json!({});
        apply_mcp_install(&mut config);
        write_json(&paths.claude_config(), &config).unwrap();
        assert_eq!(detect_configured_targets(&paths), vec![TargetId::Claude]);

        atomic_write(&paths.codex_config(), codex_mcp_block().as_bytes()).unwrap();
        assert_eq!(
            detect_configured_targets(&paths),
            vec![TargetId::Claude, TargetId::Codex]
        );

        install_opencode(&paths, SetupBackend::Jdb).unwrap();
        assert_eq!(
            detect_configured_targets(&paths),
            vec![TargetId::Claude, TargetId::Codex, TargetId::Opencode]
        );

        install_pi(&paths, SetupBackend::Jdb).unwrap();
        assert_eq!(
            detect_configured_targets(&paths),
            vec![
                TargetId::Claude,
                TargetId::Codex,
                TargetId::Opencode,
                TargetId::Pi
            ]
        );

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn install_and_remove_codex_preserves_sibling_tables() {
        let home = temp_home("codex");
        let paths = Paths::for_home(&home);
        atomic_write(
            &paths.codex_config(),
            b"[other]\nfoo = \"bar\"\n\n[tail]\nvalue = \"ok\"\n",
        )
        .unwrap();

        let written = install_codex(&paths, SetupBackend::Jdb).unwrap();
        assert_eq!(written.len(), 2);
        let after_install = fs::read_to_string(paths.codex_config()).unwrap();
        assert!(after_install.contains("[other]"));
        assert!(after_install.contains("[mcp_servers.jdbg]"));
        assert!(after_install.contains("[tail]"));
        assert!(paths.codex_skill_dir().join("SKILL.md").exists());

        let removed = remove_codex(&paths).unwrap();
        assert_eq!(removed.len(), 2);
        let after_remove = fs::read_to_string(paths.codex_config()).unwrap();
        assert!(after_remove.contains("[other]"));
        assert!(after_remove.contains("[tail]"));
        assert!(!after_remove.contains("[mcp_servers.jdbg]"));
        assert!(!paths.codex_skill_dir().exists());

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn install_and_remove_opencode_preserves_sibling_mcp_servers() {
        let home = temp_home("opencode");
        let paths = Paths::for_home(&home);
        write_json(
            &paths.opencode_config(),
            &json!({
                "mcp": {
                    "other": { "type": "local", "command": ["other-mcp"], "enabled": true }
                },
                "model": "test-model"
            }),
        )
        .unwrap();

        let written = install_opencode(&paths, SetupBackend::Jdb).unwrap();
        assert_eq!(written.len(), 2);
        let after_install = read_json_strict(&paths.opencode_config()).unwrap();
        assert_eq!(
            after_install["mcp"][MCP_SERVER_KEY],
            opencode_mcp_server_value()
        );
        assert_eq!(
            after_install["mcp"]["other"]["command"],
            json!(["other-mcp"])
        );
        assert_eq!(after_install["model"], json!("test-model"));
        assert!(paths.opencode_skill_dir().join("SKILL.md").exists());

        let removed = remove_opencode(&paths).unwrap();
        assert_eq!(removed.len(), 2);
        let after_remove = read_json_strict(&paths.opencode_config()).unwrap();
        assert!(after_remove["mcp"].get(MCP_SERVER_KEY).is_none());
        assert_eq!(
            after_remove["mcp"]["other"]["command"],
            json!(["other-mcp"])
        );
        assert!(!paths.opencode_skill_dir().exists());

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn install_and_remove_pi_writes_only_cli_skill() {
        let home = temp_home("pi");
        let paths = Paths::for_home(&home);

        let written = install_pi(&paths, SetupBackend::Jdb).unwrap();
        assert_eq!(written, vec![paths.pi_skill_dir().join("SKILL.md")]);
        let installed = fs::read_to_string(paths.pi_skill_dir().join("SKILL.md")).unwrap();
        assert_skill_frontmatter_is_pi_yaml_safe(&installed);
        assert!(installed.contains("Use the jdbg CLI"));
        assert!(!paths.pi_settings().exists());

        let removed = remove_pi(&paths).unwrap();
        assert_eq!(removed, vec![paths.pi_skill_dir()]);
        assert!(!paths.pi_skill_dir().exists());

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn skills_are_embedded() {
        assert_skill_frontmatter_is_pi_yaml_safe(MCP_SKILL_MD);
        assert!(MCP_SKILL_MD.contains("MCP server"));
        assert!(MCP_SKILL_MD.len() > 500);
        assert_skill_frontmatter_is_pi_yaml_safe(CLI_SKILL_MD);
        assert!(CLI_SKILL_MD.contains("Use the jdbg CLI"));
        assert!(CLI_SKILL_MD.contains("Pi has no official jdbg MCP setup"));
        assert!(CLI_SKILL_MD.len() > 500);
    }

    #[test]
    fn rendered_mcp_skill_records_jdi_backend_preference() {
        let rendered = render_skill(MCP_SKILL_MD, SetupBackend::Jdi);

        assert!(rendered.contains("Preferred backend: JDI"));
        assert!(rendered.contains("Omit `backend`"));
        assert!(rendered.contains(r#""backend": "jdb""#));
        assert!(rendered.contains("literal raw jdb stdin passthrough"));
    }

    #[test]
    fn install_codex_writes_selected_backend_preference_to_skill() {
        let home = temp_home("codex-backend");
        let paths = Paths::for_home(&home);

        install_codex(&paths, SetupBackend::Jdi).unwrap();

        let installed = fs::read_to_string(paths.codex_skill_dir().join("SKILL.md")).unwrap();
        assert!(installed.contains("Preferred backend: JDI"));
        assert!(installed.contains("Omit `backend`"));
        assert!(installed.contains(r#""backend": "jdb""#));

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn jdi_setup_installs_missing_sidecar_next_to_exe() {
        let home = temp_home("jdi-sidecar-missing");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(if cfg!(windows) { "jdbg.exe" } else { "jdbg" });
        let expected = bin.join(crate::jdi::lifecycle::SIDECAR_JAR_NAME);

        let installed =
            ensure_jdi_sidecar_available_with(SetupBackend::Jdi, &expected, &exe, |actual_exe| {
                assert_eq!(actual_exe, exe.as_path());
                fs::write(&expected, b"jar").unwrap();
                Ok(expected.clone())
            })
            .unwrap();

        assert_eq!(installed, Some(expected.clone()));
        assert_eq!(fs::read(expected).unwrap(), b"jar");

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn jdb_setup_does_not_install_missing_sidecar() {
        let home = temp_home("jdb-sidecar");
        let exe = home.join(if cfg!(windows) { "jdbg.exe" } else { "jdbg" });
        let expected = home.join(crate::jdi::lifecycle::SIDECAR_JAR_NAME);

        let installed =
            ensure_jdi_sidecar_available_with(SetupBackend::Jdb, &expected, &exe, |_| {
                panic!("JDB setup must not install the JDI sidecar")
            })
            .unwrap();

        assert_eq!(installed, None);

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn jdi_setup_keeps_existing_sidecar() {
        let home = temp_home("jdi-sidecar-existing");
        let exe = home.join(if cfg!(windows) { "jdbg.exe" } else { "jdbg" });
        let expected = home.join(crate::jdi::lifecycle::SIDECAR_JAR_NAME);
        fs::create_dir_all(&home).unwrap();
        fs::write(&expected, b"existing").unwrap();

        let installed =
            ensure_jdi_sidecar_available_with(SetupBackend::Jdi, &expected, &exe, |_| {
                panic!("JDI setup must not reinstall an existing sidecar")
            })
            .unwrap();

        assert_eq!(installed, None);
        assert_eq!(fs::read(expected).unwrap(), b"existing");

        let _ = fs::remove_dir_all(home);
    }

    fn assert_skill_frontmatter_is_pi_yaml_safe(content: &str) {
        let content = content.replace("\r\n", "\n");
        assert!(content.starts_with("---\n"));
        let frontmatter_end = content[4..]
            .find("\n---")
            .expect("skill should have closing frontmatter marker")
            + 4;
        let frontmatter = &content[4..frontmatter_end];

        assert!(frontmatter.lines().any(|line| line == r#"name: "jdbg""#));
        for field in ["description", "compatibility", "allowed-tools"] {
            let prefix = format!("{field}: ");
            let line = frontmatter
                .lines()
                .find(|line| line.starts_with(&prefix))
                .unwrap_or_else(|| panic!("missing {field} frontmatter"));
            let value = &line[prefix.len()..];
            assert!(
                value.starts_with('"') && value.ends_with('"'),
                "{field} must be quoted so YAML parsers accept values containing ':'"
            );
        }
    }
}
