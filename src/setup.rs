//! `jdbg setup` registers or removes jdbg for supported coding agents.
//!
//! First-version targets:
//! - Claude Code: `~/.claude.json`, `~/.claude/settings.json`, `~/.claude/skills/jdbg/`
//! - Codex: `~/.codex/config.toml`, `~/.codex/skills/jdbg/`

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const MCP_SERVER_KEY: &str = "jdbg";
const PERMISSION_ENTRY: &str = "mcp__jdbg__*";
const CODEX_TOML_HEADER: &str = "mcp_servers.jdbg";

const SKILL_MD: &str = include_str!("../skills/jdbg/SKILL.md");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetId {
    Claude,
    Codex,
}

impl TargetId {
    fn id(self) -> &'static str {
        match self {
            TargetId::Claude => "claude",
            TargetId::Codex => "codex",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            TargetId::Claude => "Claude Code",
            TargetId::Codex => "Codex",
        }
    }
}

const ALL_TARGETS: [TargetId; 2] = [TargetId::Claude, TargetId::Codex];

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

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
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

fn install_skill(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    fs::write(&path, SKILL_MD)?;
    Ok(path)
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

fn detect_installed(target: TargetId, paths: &Paths) -> bool {
    match target {
        TargetId::Claude => paths.home.join(".claude").exists() || paths.claude_config().exists(),
        TargetId::Codex => paths.codex_dir().exists() || paths.codex_config().exists(),
    }
}

fn detect_configured(target: TargetId, paths: &Paths) -> bool {
    match target {
        TargetId::Claude => detect_claude_configured(paths),
        TargetId::Codex => detect_codex_configured(paths),
    }
}

fn detect_configured_targets(paths: &Paths) -> Vec<TargetId> {
    ALL_TARGETS
        .iter()
        .copied()
        .filter(|target| detect_configured(*target, paths))
        .collect()
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
            "unknown --target id(s): {}. Known: claude, codex, plus auto/all/none",
            unknown.join(", ")
        );
    }
    Ok(targets)
}

fn prompt_targets(defaults: &[TargetId]) -> Result<Vec<TargetId>> {
    println!("Which agents should jdbg configure?");
    for (idx, target) in ALL_TARGETS.iter().enumerate() {
        let checked = if defaults.contains(target) { "x" } else { " " };
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

fn install_claude(paths: &Paths) -> Result<Vec<PathBuf>> {
    let config_path = paths.claude_config();
    let settings_path = paths.claude_settings();

    let mut config = read_json(&config_path);
    let mut settings = read_json(&settings_path);
    apply_mcp_install(&mut config);
    apply_perm_install(&mut settings);
    write_json(&config_path, &config)?;
    write_json(&settings_path, &settings)?;
    let skill_path = install_skill(&paths.claude_skill_dir())?;

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

fn install_codex(paths: &Paths) -> Result<Vec<PathBuf>> {
    let config_path = paths.codex_config();
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    let (next, changed) = upsert_toml_table(&existing, CODEX_TOML_HEADER, &codex_mcp_block());
    if changed {
        atomic_write(&config_path, next.as_bytes())?;
    }
    let skill_path = install_skill(&paths.codex_skill_dir())?;
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

fn print_target_config(target: TargetId, paths: &Paths) -> Result<()> {
    match target {
        TargetId::Claude => {
            let snippet = json!({ "mcpServers": { MCP_SERVER_KEY: mcp_server_value() } });
            println!("# Claude Code: add to {}", paths.claude_config().display());
            println!("{}", serde_json::to_string_pretty(&snippet)?);
        }
        TargetId::Codex => {
            println!("# Codex: add to {}", paths.codex_config().display());
            println!("{}", codex_mcp_block());
        }
    }
    Ok(())
}

pub fn run_setup(remove: bool, print: bool, target: Option<&str>, yes: bool) -> Result<()> {
    let paths = Paths::new()?;
    let targets = if print {
        match target {
            Some(value) => parse_target_flag(value, &paths)?,
            None => vec![TargetId::Claude],
        }
    } else {
        select_targets(remove, target, yes, &paths)?
    };

    if print {
        for (idx, target) in targets.iter().enumerate() {
            if idx > 0 {
                println!();
            }
            print_target_config(*target, &paths)?;
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
        for target in targets {
            let written = match target {
                TargetId::Claude => install_claude(&paths)?,
                TargetId::Codex => install_codex(&paths)?,
            };
            println!("Registered jdbg for {}.", target.display_name());
            for path in written {
                println!("  {}", path.display());
            }
        }
        println!("Restart or reload the configured agent(s) to pick up the jdbg MCP server.");
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
            parse_target_flag("claude,codex,claude", &paths).unwrap(),
            vec![TargetId::Claude, TargetId::Codex]
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
    fn detect_configured_finds_claude_and_codex() {
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

        let written = install_codex(&paths).unwrap();
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
    fn skill_is_embedded() {
        assert!(SKILL_MD.contains("name: jdbg"));
        assert!(SKILL_MD.contains("interactive Java debugging"));
        assert!(SKILL_MD.len() > 500);
    }
}
