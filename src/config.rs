use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml::Value;

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 3;
const DEFAULT_SCAN_CONCURRENCY: usize = 32;
pub(crate) const DEFAULT_MOUSE_SCROLL_LINES: usize = 5;
pub(crate) const DEFAULT_AUTO_REFRESH_SECS: u64 = 15;
pub(crate) const DEFAULT_EXPAND_LEVEL: ExpandLevel = ExpandLevel::Server;
pub(crate) const DEFAULT_SERVER_LINE_TEXT: &str = "[Server] {server_name}";
pub(crate) const DEFAULT_SESSION_LINE_TEXT: &str = "[Session] {session_name}";
pub(crate) const DEFAULT_WINDOW_LINE_TEXT: &str =
    "[Window] {is_active}{window_index}: {window_name}";
pub(crate) const DEFAULT_PANE_LINE_TEXT: &str =
    "[Pane] {is_active}{pane_index} {pane_id} {process_elapsed_time} {pane_commandline}";

#[derive(Debug, Deserialize)]
pub(crate) struct RawConfig {
    pub(crate) hosts: Option<Value>,
    pub(crate) connect_timeout_secs: Option<u64>,
    pub(crate) scan_concurrency: Option<usize>,
    pub(crate) mouse_scroll_lines: Option<usize>,
    pub(crate) auto_refresh_secs: Option<u64>,
    pub(crate) default_expand_level: Option<String>,
    pub(crate) log_path: Option<PathBuf>,
    pub(crate) server_line_text: Option<String>,
    pub(crate) session_line_text: Option<String>,
    pub(crate) window_line_text: Option<String>,
    pub(crate) pane_line_text: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) hosts: Vec<String>,
    pub(crate) connect_timeout_secs: u64,
    pub(crate) scan_concurrency: usize,
    pub(crate) mouse_scroll_lines: usize,
    pub(crate) auto_refresh_secs: u64,
    pub(crate) default_expand_level: ExpandLevel,
    pub(crate) log_path: Option<PathBuf>,
    pub(crate) line_formats: LineFormats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpandLevel {
    Server,
    Session,
    Window,
    Pane,
}

#[derive(Clone, Debug)]
pub(crate) struct LineFormats {
    pub(crate) server: String,
    pub(crate) session: String,
    pub(crate) window: String,
    pub(crate) pane: String,
}

pub(crate) fn default_config_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("tmux-gateway")
            .join("config.toml"),
        None => PathBuf::from("config.toml"),
    }
}

pub(crate) fn load_config(path: &PathBuf) -> Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        fs::write(path, "")
            .with_context(|| format!("failed to create empty config {}", path.display()))?;
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let raw: RawConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let config = normalize_config(raw)?;

    Ok(config)
}

pub(crate) fn normalize_config(raw: RawConfig) -> Result<Config> {
    let hosts_value = raw
        .hosts
        .unwrap_or_else(|| Value::String("all".to_string()));
    let hosts = match hosts_value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(host) => Ok(host),
                other => bail!("hosts array must contain only strings, got {other:?}"),
            })
            .collect::<Result<Vec<_>>>()?,
        Value::String(value) if value == "all" => load_ssh_config_hosts()?,
        Value::String(value) => bail!("unsupported hosts string {value:?}; expected \"all\""),
        other => bail!("hosts must be an array of strings or the string \"all\", got {other:?}"),
    };

    let connect_timeout_secs = raw
        .connect_timeout_secs
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);
    let scan_concurrency = raw
        .scan_concurrency
        .unwrap_or(DEFAULT_SCAN_CONCURRENCY)
        .max(1);
    let mouse_scroll_lines = raw
        .mouse_scroll_lines
        .unwrap_or(DEFAULT_MOUSE_SCROLL_LINES)
        .max(1);
    let auto_refresh_secs = raw.auto_refresh_secs.unwrap_or(DEFAULT_AUTO_REFRESH_SECS);
    let default_expand_level = parse_expand_level(raw.default_expand_level.as_deref())?;

    Ok(Config {
        hosts: dedup_hosts(hosts),
        connect_timeout_secs,
        scan_concurrency,
        mouse_scroll_lines,
        auto_refresh_secs,
        default_expand_level,
        log_path: raw.log_path,
        line_formats: LineFormats {
            server: raw
                .server_line_text
                .unwrap_or_else(|| DEFAULT_SERVER_LINE_TEXT.to_string()),
            session: raw
                .session_line_text
                .unwrap_or_else(|| DEFAULT_SESSION_LINE_TEXT.to_string()),
            window: raw
                .window_line_text
                .unwrap_or_else(|| DEFAULT_WINDOW_LINE_TEXT.to_string()),
            pane: raw
                .pane_line_text
                .unwrap_or_else(|| DEFAULT_PANE_LINE_TEXT.to_string()),
        },
    })
}

fn parse_expand_level(value: Option<&str>) -> Result<ExpandLevel> {
    let Some(value) = value else {
        return Ok(DEFAULT_EXPAND_LEVEL);
    };

    match value.trim().to_lowercase().as_str() {
        "server" => Ok(ExpandLevel::Server),
        "session" => Ok(ExpandLevel::Session),
        "window" => Ok(ExpandLevel::Window),
        "pane" => Ok(ExpandLevel::Pane),
        other => bail!(
            "unsupported default_expand_level {other:?}; expected server, session, window, or pane"
        ),
    }
}

fn load_ssh_config_hosts() -> Result<Vec<String>> {
    let home = std::env::var("HOME").context("HOME is not set; cannot read ~/.ssh/config")?;
    let path = PathBuf::from(home).join(".ssh").join("config");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read ssh config {}", path.display()))?;
    Ok(parse_ssh_config_hosts(&content))
}

pub(crate) fn parse_ssh_config_hosts(content: &str) -> Vec<String> {
    let mut hosts = Vec::new();

    for line in content.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }

        for pattern in parts {
            if is_concrete_ssh_host(pattern) {
                hosts.push(pattern.to_string());
            }
        }
    }

    dedup_hosts(hosts)
}

fn is_concrete_ssh_host(pattern: &str) -> bool {
    !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.contains('[')
        && !pattern.contains(']')
}

fn dedup_hosts(hosts: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();

    for host in hosts {
        if seen.insert(host.clone()) {
            deduped.push(host);
        }
    }

    deduped
}
