//! `jdbg setup` — 注册/卸载 Claude Code MCP server 配置。
//!
//! 操作两个文件（跨平台，通过 `directories::BaseDirs` 定位 home）：
//! - `~/.claude.json` — mcpServers.jdbg
//! - `~/.claude/settings.json` — permissions.allow "mcp__jdbg__*"

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

// ─── 常量 ───────────────────────────────────────────────────────────────────

const MCP_SERVER_KEY: &str = "jdbg";
const PERMISSION_ENTRY: &str = "mcp__jdbg__*";

/// skill 文档，编译期内嵌进二进制——保证 prebuilt 安装（机器上没有仓库 skills/ 目录）也带 skill。
const SKILL_MD: &str = include_str!("../skills/jdbg/SKILL.md");

fn mcp_server_value() -> Value {
    json!({
        "command": "jdbg",
        "args": ["__mcp"]
    })
}

// ─── 路径定位 ────────────────────────────────────────────────────────────────

fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("cannot determine home directory")
}

/// `~/.claude.json`
pub fn claude_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude.json"))
}

/// `~/.claude/settings.json`
pub fn claude_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

/// `~/.claude/skills/jdbg/`
pub fn skill_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("skills").join("jdbg"))
}

// ─── JSON 读写 ───────────────────────────────────────────────────────────────

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// PLACEHOLDER_WRITE_IMPL
fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content.as_bytes())?;
    fs::rename(&tmp, path).context("atomic rename failed")?;
    Ok(())
}

// ─── skill 安装/卸载 ─────────────────────────────────────────────────────────

/// 把内嵌的 skill 写入全局 skill 目录，返回写入路径。
fn install_skill() -> Result<PathBuf> {
    let dir = skill_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    fs::write(&path, SKILL_MD)?;
    Ok(path)
}

/// 删除全局 skill 目录；返回是否确有删除。
fn remove_skill() -> Result<bool> {
    let dir = skill_dir()?;
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ─── 纯逻辑：mcpServers 增删 ──────────────────────────────────────────────────

/// 在 config 的 `mcpServers` 下写入 jdbg 条目（object 不存在则创建）。
fn apply_mcp_install(config: &mut Value) {
    let servers = config
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_KEY.to_owned(), mcp_server_value());
}

/// 从 config 的 `mcpServers` 删除 jdbg 条目；object 变空则删 key。
/// 返回是否确有删除。
fn apply_mcp_remove(config: &mut Value) -> bool {
    let Some(obj) = config.as_object_mut() else { return false };
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

// ─── 纯逻辑：permissions.allow 增删 ───────────────────────────────────────────

/// 在 settings 的 `permissions.allow` 数组追加放行项（已存在则不动）。
fn apply_perm_install(settings: &mut Value) {
    let perms = settings
        .as_object_mut()
        .unwrap()
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let allow = perms
        .as_object_mut()
        .unwrap()
        .entry("allow")
        .or_insert_with(|| json!([]));
    let arr = allow.as_array_mut().unwrap();
    let entry = json!(PERMISSION_ENTRY);
    if !arr.contains(&entry) {
        arr.push(entry);
    }
}

/// 从 settings 的 `permissions.allow` 移除放行项；数组/对象变空则清理。
/// 返回是否确有删除。
fn apply_perm_remove(settings: &mut Value) -> bool {
    let Some(obj) = settings.as_object_mut() else { return false };
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

// ─── 编排 ────────────────────────────────────────────────────────────────────

/// `jdbg setup` 入口。
pub fn run_setup(remove: bool, print: bool) -> Result<()> {
    if print {
        let snippet = json!({ "mcpServers": { MCP_SERVER_KEY: mcp_server_value() } });
        println!("{}", serde_json::to_string_pretty(&snippet)?);
        return Ok(());
    }

    let config_path = claude_config_path()?;
    let settings_path = claude_settings_path()?;

    if remove {
        let mut config = read_json(&config_path);
        let mut settings = read_json(&settings_path);
        let c = apply_mcp_remove(&mut config);
        let p = apply_perm_remove(&mut settings);
        if c {
            write_json(&config_path, &config)?;
        }
        if p {
            write_json(&settings_path, &settings)?;
        }
        let s = remove_skill()?;
        if c || p || s {
            println!("✓ Removed jdbg MCP server and skill from Claude Code config.");
        } else {
            println!("jdbg was not registered — nothing to remove.");
        }
    } else {
        let mut config = read_json(&config_path);
        let mut settings = read_json(&settings_path);
        apply_mcp_install(&mut config);
        apply_perm_install(&mut settings);
        write_json(&config_path, &config)?;
        write_json(&settings_path, &settings)?;
        let skill_path = install_skill()?;
        println!("✓ Registered jdbg MCP server and skill for Claude Code.");
        println!("  config:   {}", config_path.display());
        println!("  settings: {}", settings_path.display());
        println!("  skill:    {}", skill_path.display());
        println!("  Restart Claude Code (or reload) to pick up the new server.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // jdbg 写入
        assert_eq!(config["mcpServers"][MCP_SERVER_KEY], mcp_server_value());
        // 其它 server 与顶层 key 不动
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
        // mcpServers 变空 → 整个 key 删掉
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
        apply_perm_install(&mut settings); // 二次不重复
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
        // allow 空 → permissions 也清掉
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
    fn skill_is_embedded() {
        // include_str! 必须在编译期把 SKILL.md 打进二进制（prebuilt 安装不带仓库文件）。
        assert!(SKILL_MD.contains("name: jdbg"));
        assert!(SKILL_MD.contains("interactive Java debugging"));
        assert!(SKILL_MD.len() > 500);
    }
}

