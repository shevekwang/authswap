#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use clap::Parser;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, DisableLineWrap, EnableLineWrap,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color as TuiColor, Modifier as TuiModifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear as TuiClear, Paragraph, Row, Table, Wrap};
use ratatui::Terminal as RatatuiTerminal;
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::{Method, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use walkdir::WalkDir;

const VERSION: &str = "1.0.2";
const MANIFEST_NAME: &str = "manifest.json";
const DEFAULT_CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

#[derive(Parser)]
#[command(
    name = "authswap",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountRecord {
    #[serde(default)]
    account_key: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    alias: String,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    last_used_at: Option<i64>,
    #[serde(default)]
    last_usage: Option<RateLimitSnapshot>,
    #[serde(default)]
    last_usage_at: Option<i64>,
    #[serde(default)]
    inactive: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RateLimitSnapshot {
    #[serde(default)]
    primary: Option<RateLimitWindow>,
    #[serde(default)]
    secondary: Option<RateLimitWindow>,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RateLimitWindow {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<i64>,
    #[serde(default)]
    resets_at: Option<i64>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registry {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    active_account_key: Option<String>,
    #[serde(default)]
    active_account_activated_at_ms: Option<i64>,
    #[serde(default)]
    accounts: Vec<AccountRecord>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    #[serde(rename = "generatedAt")]
    generated_at: String,
    #[serde(rename = "codexHome")]
    codex_home: String,
    files: Vec<FileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileRecord {
    path: String,
    size: u64,
    sha256: String,
    #[serde(rename = "mtimeMs")]
    mtime_ms: i64,
}

struct SyncFile {
    relative_path: String,
    absolute_path: PathBuf,
    data: Vec<u8>,
}

struct WebdavConfig {
    base_url: String,
    username: Option<String>,
    password: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppSettings {
    #[serde(default)]
    webdav: WebdavSettings,
    #[serde(default)]
    restart_app_server_after_switch: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebdavSettings {
    #[serde(default)]
    url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthSnapshot {
    tokens: AuthTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthTokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
struct ImportedAccount {
    auth: Value,
    account_key: String,
    email: String,
    account_name: Option<String>,
    plan: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerAction {
    Add,
    EditSettings,
    Sync,
    Switch(usize),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunAction {
    AddAccount,
    WebdavSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebdavAction {
    Push,
    Pull,
    Configure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddMethod {
    Oauth,
    DeviceAuth,
    Json,
}

#[derive(Debug, Deserialize)]
struct CodexUsageResponse {
    #[serde(default)]
    plan_type: Option<String>,
    rate_limit: CodexUsageRateLimit,
}

#[derive(Debug, Deserialize)]
struct CodexUsageRateLimit {
    primary_window: CodexUsageWindow,
    secondary_window: CodexUsageWindow,
}

#[derive(Debug, Deserialize)]
struct CodexUsageWindow {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

#[derive(Debug)]
enum CodexUsageError {
    Unauthorized(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for CodexUsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized(message) => formatter.write_str(message),
            Self::Other(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for CodexUsageError {}

#[derive(Debug, Clone)]
struct AccountDisplayRow {
    index: usize,
    active: bool,
    inactive: bool,
    account: String,
    email: String,
    plan: String,
    limit_5h: String,
    limit_week: String,
}

#[derive(Debug, Clone, Copy)]
struct AccountTableWidths {
    account: usize,
    email: usize,
    plan: usize,
    limit_5h: usize,
    limit_week: usize,
}

#[derive(Debug, Clone, Copy)]
struct PickerCells<'a> {
    account: &'a str,
    email: &'a str,
    plan: &'a str,
    limit_5h: &'a str,
    limit_week: &'a str,
}

fn default_schema_version() -> u32 {
    3
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let _cli = Cli::parse();
    interactive_switch()
}

fn codex_home() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".codex")
        })
}

fn accounts_dir() -> PathBuf {
    codex_home().join("accounts")
}

fn registry_path() -> PathBuf {
    accounts_dir().join("registry.json")
}

fn auth_path() -> PathBuf {
    codex_home().join("auth.json")
}

fn config_path() -> PathBuf {
    codex_home().join("config.toml")
}

fn settings_path() -> PathBuf {
    codex_home().join("authswap.json")
}

fn read_registry() -> Result<Registry> {
    let path = registry_path();
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_registry_or_default() -> Result<Registry> {
    let path = registry_path();
    if !path.exists() {
        return Ok(Registry {
            schema_version: default_schema_version(),
            active_account_key: None,
            active_account_activated_at_ms: None,
            accounts: Vec::new(),
            extra: BTreeMap::new(),
        });
    }
    read_registry()
}

fn write_registry(registry: &Registry) -> Result<()> {
    let path = registry_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("registry path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(registry)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn read_settings() -> Result<AppSettings> {
    let path = settings_path();
    if !path.exists() {
        return Ok(AppSettings::default());
    }
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_settings(settings: &AppSettings) -> Result<()> {
    let path = settings_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("settings path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
    set_private_file_permissions(&tmp)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn print_accounts(registry: &Registry) -> Result<()> {
    let rows = account_display_rows(registry)?;
    println!(
        "{:<2} {:>4} {:<30} {:<12} {:<18} {:<18}",
        "", "idx", "account", "plan", "5h limit", "week limit"
    );
    for row in rows {
        let active = if row.inactive {
            "!"
        } else if row.active {
            "*"
        } else {
            " "
        };
        println!(
            "{:<2} {:>3}. {:<30} {:<12} {:<18} {:<18}",
            active,
            row.index + 1,
            row.account,
            row.plan,
            row.limit_5h,
            row.limit_week
        );
    }
    Ok(())
}

fn interactive_switch() -> Result<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        loop {
            let mut registry = read_registry_or_default()?;
            reconcile_active_account_from_current_auth(&mut registry)?;
            sync_active_auth_snapshot(&registry)?;
            if registry.accounts.is_empty() {
                match choose_first_run_action()? {
                    Some(FirstRunAction::AddAccount) => {
                        if !interactive_add_account(None)? {
                            return Ok(());
                        }
                    }
                    Some(FirstRunAction::WebdavSync) => interactive_webdav_sync()?,
                    None => return Ok(()),
                }
                continue;
            }
            match interactive_account_picker(&mut registry)? {
                PickerAction::Add => {
                    let _ = interactive_add_account(None)?;
                }
                PickerAction::EditSettings => {
                    interactive_app_server_restart_setting()?;
                }
                PickerAction::Sync => {
                    interactive_webdav_sync()?;
                }
                PickerAction::Switch(index) => {
                    return switch_account(Some(registry.accounts[index].account_key.clone()));
                }
                PickerAction::Quit => return Ok(()),
            }
        }
    }
    let mut registry = read_registry()?;
    reconcile_active_account_from_current_auth(&mut registry)?;
    sync_active_auth_snapshot(&registry)?;
    if registry.accounts.is_empty() {
        bail!("no accounts found");
    }
    interactive_switch_line_mode(&registry)
}

fn interactive_switch_line_mode(registry: &Registry) -> Result<()> {
    print_accounts(registry)?;
    println!();
    print!("Select account number or email (q to quit): ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("q") {
        return Ok(());
    }
    if let Ok(index) = input.parse::<usize>() {
        let Some(account) = registry.accounts.get(index.saturating_sub(1)) else {
            bail!("selection out of range: {index}");
        };
        return switch_account(Some(account.account_key.clone()));
    }
    switch_account(Some(input.to_string()))
}

fn interactive_account_picker(registry: &mut Registry) -> Result<PickerAction> {
    let mut terminal = TerminalSession::enter()?;
    let mut selected = active_account_index(registry).unwrap_or(0);
    let mut number_buffer = String::new();
    let mut status_message: Option<String> = None;
    loop {
        if registry.accounts.is_empty() {
            return Ok(PickerAction::Quit);
        }
        selected = selected.min(registry.accounts.len() - 1);
        let rows = account_display_rows(registry)?;
        render_account_picker(
            &mut terminal.stdout,
            &rows,
            selected,
            &number_buffer,
            status_message.as_deref(),
        )?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                return Ok(PickerAction::Quit)
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                interactive_add_account_overlay(&mut terminal.stdout)?;
                *registry = read_registry_or_default()?;
                selected = active_account_index(registry).unwrap_or(0);
                number_buffer.clear();
                status_message = None;
                render_picker_background(&mut terminal.stdout, registry, selected, None)?;
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                interactive_app_server_restart_setting_overlay(&mut terminal.stdout)?;
                number_buffer.clear();
                status_message = None;
                render_picker_background(&mut terminal.stdout, registry, selected, None)?;
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                interactive_webdav_sync_overlay(&mut terminal.stdout, registry, selected)?;
                *registry = read_registry_or_default()?;
                reconcile_active_account_from_current_auth(registry)?;
                selected = active_account_index(registry)
                    .unwrap_or_else(|| selected.min(registry.accounts.len().saturating_sub(1)));
                number_buffer.clear();
                status_message = None;
                render_picker_background(&mut terminal.stdout, registry, selected, None)?;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                selected = selected.saturating_sub(1);
                number_buffer.clear();
                status_message = None;
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                if selected + 1 < registry.accounts.len() {
                    selected += 1;
                }
                number_buffer.clear();
                status_message = None;
            }
            KeyCode::Home => {
                selected = 0;
                number_buffer.clear();
                status_message = None;
            }
            KeyCode::End => {
                selected = registry.accounts.len().saturating_sub(1);
                number_buffer.clear();
                status_message = None;
            }
            KeyCode::Enter => {
                if !number_buffer.is_empty() {
                    if let Ok(index) = number_buffer.parse::<usize>() {
                        if let Some(account) = registry.accounts.get(index.saturating_sub(1)) {
                            let index = registry
                                .accounts
                                .iter()
                                .position(|candidate| candidate.account_key == account.account_key)
                                .unwrap_or(selected);
                            return Ok(PickerAction::Switch(index));
                        }
                    }
                }
                return Ok(PickerAction::Switch(selected));
            }
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('h')
                if !matches!(key.code, KeyCode::Char('h'))
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                status_message = None;
                if !number_buffer.is_empty() {
                    number_buffer.pop();
                    if !number_buffer.is_empty() {
                        if let Ok(index) = number_buffer.parse::<usize>() {
                            if (1..=registry.accounts.len()).contains(&index) {
                                selected = index - 1;
                            }
                        }
                    }
                    continue;
                }
                let account = registry.accounts[selected].clone();
                let label = display_account(&account);
                if confirm_delete_account(&mut terminal.stdout, &label)? {
                    delete_account_at(registry, selected)?;
                    selected = selected.saturating_sub(1);
                }
                render_picker_background(&mut terminal.stdout, registry, selected, None)?;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                number_buffer.clear();
                let rows = account_display_rows(registry)?;
                render_account_picker(
                    &mut terminal.stdout,
                    &rows,
                    selected,
                    "",
                    Some("Refreshing selected account usage..."),
                )?;
                terminal.stdout.flush()?;
                status_message = match refresh_account_usage_at(registry, selected) {
                    Ok(()) => Some("Usage refreshed.".to_string()),
                    Err(err) => Some(format!("Usage refresh failed: {err:#}")),
                };
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                number_buffer.clear();
                let rows = account_display_rows(registry)?;
                render_account_picker(
                    &mut terminal.stdout,
                    &rows,
                    selected,
                    "",
                    Some("Refreshing usage for all accounts..."),
                )?;
                terminal.stdout.flush()?;
                status_message = Some(refresh_all_account_usage(registry)?);
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() && number_buffer.len() < 8 => {
                number_buffer.push(ch);
                if let Ok(index) = number_buffer.parse::<usize>() {
                    if (1..=registry.accounts.len()).contains(&index) {
                        selected = index - 1;
                    }
                }
                status_message = None;
            }
            _ => {}
        }
    }
}

fn render_picker_background(
    stdout: &mut std::io::Stdout,
    registry: &Registry,
    selected: usize,
    footer: Option<&str>,
) -> Result<()> {
    let rows = account_display_rows(registry)?;
    queue!(stdout, Clear(ClearType::All))?;
    stdout.flush()?;
    render_account_picker(stdout, &rows, selected, "", footer)
}

fn interactive_add_account_overlay(stdout: &mut std::io::Stdout) -> Result<()> {
    let Some(method) = choose_add_method_overlay(stdout)? else {
        return Ok(());
    };
    match method {
        AddMethod::Oauth => run_external_account_import(stdout, false, None),
        AddMethod::DeviceAuth => run_external_account_import(stdout, true, None),
        AddMethod::Json => {
            let Some(input) = read_json_path_overlay(stdout)? else {
                return Ok(());
            };
            let path = sanitize_json_path_input(&input)?;
            let data = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let imported = import_json_accounts(&data, None)?;
            persist_imported_accounts(imported)
        }
    }
}

fn choose_add_method_overlay(stdout: &mut std::io::Stdout) -> Result<Option<AddMethod>> {
    let mut selected = 0usize;
    loop {
        render_add_account_dialog(stdout, selected)?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(None),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                selected = (selected + 1).min(2);
            }
            KeyCode::Char('1') | KeyCode::Char('o') | KeyCode::Char('O') => {
                return Ok(Some(AddMethod::Oauth));
            }
            KeyCode::Char('2') | KeyCode::Char('d') | KeyCode::Char('D') => {
                return Ok(Some(AddMethod::DeviceAuth));
            }
            KeyCode::Char('3') | KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('J') => {
                return Ok(Some(AddMethod::Json));
            }
            KeyCode::Enter => {
                return Ok(Some(match selected {
                    0 => AddMethod::Oauth,
                    1 => AddMethod::DeviceAuth,
                    _ => AddMethod::Json,
                }));
            }
            _ => {}
        }
    }
}

fn read_json_path_overlay(stdout: &mut std::io::Stdout) -> Result<Option<String>> {
    let mut input = String::new();
    loop {
        render_json_path_input_dialog(stdout, &input)?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => return Ok(Some(input)),
            KeyCode::Tab => {}
            KeyCode::Char(ch) => input.push(ch),
            _ => {}
        }
    }
}

fn run_external_account_import(
    stdout: &mut std::io::Stdout,
    device_auth: bool,
    alias: Option<&str>,
) -> Result<()> {
    suspend_terminal(stdout)?;
    let result = add_account_from_codex_login(device_auth, alias);
    let wait_result = wait_for_enter();
    resume_terminal(stdout)?;
    result?;
    wait_result
}

fn interactive_app_server_restart_setting_overlay(stdout: &mut std::io::Stdout) -> Result<()> {
    let mut settings = read_settings()?;
    let mut enabled = settings.restart_app_server_after_switch;
    loop {
        render_app_server_restart_dialog(stdout, enabled)?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::Char(' ')
            | KeyCode::Char('e')
            | KeyCode::Char('E') => {
                enabled = !enabled;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                enabled = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                enabled = false;
            }
            KeyCode::Enter => {
                settings.restart_app_server_after_switch = enabled;
                write_settings(&settings)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn interactive_webdav_sync_overlay(
    stdout: &mut std::io::Stdout,
    registry: &Registry,
    selected_account: usize,
) -> Result<()> {
    if !has_saved_webdav_config()? && !configure_webdav_overlay(stdout)? {
        return Ok(());
    }
    loop {
        let reachable = webdav_connection_status().is_ok();
        let Some(action) =
            choose_webdav_action_overlay(stdout, registry, selected_account, reachable)?
        else {
            return Ok(());
        };
        match action {
            WebdavAction::Push => {
                suspend_terminal(stdout)?;
                let result = cloud_push(true).map(|_| println!("WebDAV push completed."));
                let wait_result = wait_for_enter();
                resume_terminal(stdout)?;
                result?;
                wait_result?;
                return Ok(());
            }
            WebdavAction::Pull => {
                suspend_terminal(stdout)?;
                let result = cloud_pull(true).map(|_| println!("WebDAV pull completed."));
                let wait_result = wait_for_enter();
                resume_terminal(stdout)?;
                result?;
                wait_result?;
                return Ok(());
            }
            WebdavAction::Configure => {
                let _ = configure_webdav_overlay(stdout)?;
                render_picker_background(stdout, registry, selected_account, None)?;
            }
        }
    }
}

fn choose_webdav_action_overlay(
    stdout: &mut std::io::Stdout,
    registry: &Registry,
    selected_account: usize,
    reachable: bool,
) -> Result<Option<WebdavAction>> {
    let mut selected = 0usize;
    loop {
        render_webdav_dialog_overlay(stdout, registry, selected_account, selected, reachable)?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(None),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                selected = (selected + 1).min(2);
            }
            KeyCode::Char('1') | KeyCode::Char('p') | KeyCode::Char('P') => {
                return Ok(Some(WebdavAction::Push));
            }
            KeyCode::Char('2') | KeyCode::Char('g') | KeyCode::Char('G') => {
                return Ok(Some(WebdavAction::Pull));
            }
            KeyCode::Char('3') | KeyCode::Char('c') | KeyCode::Char('C') => {
                return Ok(Some(WebdavAction::Configure));
            }
            KeyCode::Enter => {
                return Ok(Some(match selected {
                    0 => WebdavAction::Push,
                    1 => WebdavAction::Pull,
                    _ => WebdavAction::Configure,
                }));
            }
            _ => {}
        }
    }
}

fn configure_webdav_overlay(stdout: &mut std::io::Stdout) -> Result<bool> {
    let mut settings = read_settings()?;
    let Some(webdav) = interactive_webdav_config_overlay(stdout, settings.webdav.clone())? else {
        return Ok(false);
    };
    settings.webdav = webdav;
    write_settings(&settings)?;
    Ok(true)
}

fn interactive_webdav_config_overlay(
    stdout: &mut std::io::Stdout,
    initial: WebdavSettings,
) -> Result<Option<WebdavSettings>> {
    let mut fields = [initial.url, initial.username, initial.password];
    let mut selected = 0usize;
    let mut status_message: Option<String> = None;
    loop {
        render_webdav_config_dialog(stdout, &fields, selected, status_message.as_deref())?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                status_message = None;
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1).min(fields.len() - 1);
                status_message = None;
            }
            KeyCode::BackTab => {
                selected = selected.saturating_sub(1);
                status_message = None;
            }
            KeyCode::Backspace => {
                fields[selected].pop();
                status_message = None;
            }
            KeyCode::Delete => {
                fields[selected].clear();
                status_message = None;
            }
            KeyCode::Enter => {
                if fields[0].trim().is_empty() {
                    selected = 0;
                    status_message = Some("WebDAV URL is required.".to_string());
                    continue;
                }
                let webdav = WebdavSettings {
                    url: normalize_webdav_url(&fields[0]),
                    username: fields[1].trim().to_string(),
                    password: fields[2].trim().to_string(),
                };
                match webdav_config_from_settings(&webdav).and_then(|config| {
                    check_webdav_config(&config).context("WebDAV connectivity test failed")
                }) {
                    Ok(()) => return Ok(Some(webdav)),
                    Err(error) => status_message = Some(error.to_string()),
                }
            }
            KeyCode::Char(ch) => {
                fields[selected].push(ch);
                status_message = None;
            }
            _ => {}
        }
    }
}

fn suspend_terminal(stdout: &mut std::io::Stdout) -> Result<()> {
    execute!(stdout, Show, EnableLineWrap, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn resume_terminal(stdout: &mut std::io::Stdout) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, DisableLineWrap, Hide)?;
    Ok(())
}

fn interactive_add_account(alias: Option<String>) -> Result<bool> {
    let Some(method) = choose_add_method()? else {
        return Ok(false);
    };
    match method {
        AddMethod::Oauth => add_account_from_codex_login(false, alias.as_deref()),
        AddMethod::DeviceAuth => add_account_from_codex_login(true, alias.as_deref()),
        AddMethod::Json => {
            let Some(input) = read_json_path_interactive()? else {
                return Ok(false);
            };
            let path = sanitize_json_path_input(&input)?;
            let data = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let imported = import_json_accounts(&data, alias.as_deref())?;
            persist_imported_accounts(imported)
        }
    }?;
    Ok(true)
}

fn choose_first_run_action() -> Result<Option<FirstRunAction>> {
    let mut terminal = TerminalSession::enter()?;
    let mut selected = 0usize;
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_first_run_dialog(&mut terminal.stdout, selected)?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(None),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                selected = (selected + 1).min(1);
            }
            KeyCode::Char('1') | KeyCode::Char('a') | KeyCode::Char('A') => {
                return Ok(Some(FirstRunAction::AddAccount));
            }
            KeyCode::Char('2') | KeyCode::Char('w') | KeyCode::Char('W') => {
                return Ok(Some(FirstRunAction::WebdavSync));
            }
            KeyCode::Enter => {
                return Ok(Some(if selected == 0 {
                    FirstRunAction::AddAccount
                } else {
                    FirstRunAction::WebdavSync
                }));
            }
            _ => {}
        }
    }
}

fn choose_add_method() -> Result<Option<AddMethod>> {
    let mut terminal = TerminalSession::enter()?;
    let mut selected = 0usize;
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_add_account_dialog(&mut terminal.stdout, selected)?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(None),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                selected = (selected + 1).min(2);
            }
            KeyCode::Char('1') | KeyCode::Char('o') | KeyCode::Char('O') => {
                return Ok(Some(AddMethod::Oauth));
            }
            KeyCode::Char('2') | KeyCode::Char('d') | KeyCode::Char('D') => {
                return Ok(Some(AddMethod::DeviceAuth));
            }
            KeyCode::Char('3') | KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('J') => {
                return Ok(Some(AddMethod::Json));
            }
            KeyCode::Enter => {
                return Ok(Some(match selected {
                    0 => AddMethod::Oauth,
                    1 => AddMethod::DeviceAuth,
                    _ => AddMethod::Json,
                }));
            }
            _ => {}
        }
    }
}

fn read_json_path_interactive() -> Result<Option<String>> {
    let mut terminal = TerminalSession::enter()?;
    let mut input = String::new();
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_json_path_input_dialog(&mut terminal.stdout, &input)?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => return Ok(Some(input)),
            KeyCode::Tab => {}
            KeyCode::Char(ch) => input.push(ch),
            _ => {}
        }
    }
}

fn sanitize_json_path_input(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("account JSON path is empty");
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    Ok(PathBuf::from(unquoted))
}

fn add_account_from_codex_login(device_auth: bool, alias: Option<&str>) -> Result<()> {
    run_codex_login(device_auth)?;
    let auth_path = auth_path();
    let data = fs::read_to_string(&auth_path)
        .with_context(|| format!("failed to read {}", auth_path.display()))?;
    let imported = import_json_accounts(&data, alias)?;
    persist_imported_accounts(imported)
}

fn run_codex_login(device_auth: bool) -> Result<()> {
    let mut command = Command::new("codex");
    command.arg("login");
    if device_auth {
        command.arg("--device-auth");
    }
    let status = command.status().with_context(|| {
        if device_auth {
            "failed to run `codex login --device-auth`"
        } else {
            "failed to run `codex login`"
        }
    })?;
    if !status.success() {
        if device_auth {
            bail!("`codex login --device-auth` failed with status {status}");
        }
        bail!("`codex login` failed with status {status}");
    }
    Ok(())
}

fn render_json_path_input_dialog(stdout: &mut std::io::Stdout, input: &str) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 96, 10);
        frame.render_widget(TuiClear, area);

        let block = Block::default()
            .title(" Import account JSON file ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(TuiColor::White))
            .style(Style::default().fg(TuiColor::White).bg(TuiColor::Black));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let help =
            Paragraph::new("Enter a JSON file path. Relative paths use the current directory.")
                .wrap(Wrap { trim: true });
        frame.render_widget(
            help,
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );

        let input_area = Rect {
            x: inner.x,
            y: inner.y.saturating_add(3),
            width: inner.width,
            height: 3,
        };
        let input_text = if input.is_empty() {
            " ".to_string()
        } else {
            truncate_display(input, input_area.width.saturating_sub(4) as usize)
        };
        let input = Paragraph::new(input_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TuiColor::Green)),
        );
        frame.render_widget(input, input_area);

        let footer = Paragraph::new("Enter imports, Esc cancels.")
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(TuiColor::Gray)
                    .add_modifier(TuiModifier::BOLD),
            );
        frame.render_widget(
            footer,
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(inner.height.saturating_sub(1)),
                width: inner.width,
                height: 1,
            },
        );
    })?;
    Ok(())
}

fn centered_popup_rect(area: Rect, max_width: u16, height: u16) -> Rect {
    let width = area.width.min(max_width).max(area.width.min(32));
    let height = area.height.min(height).max(area.height.min(8));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn render_popup_frame(frame: &mut ratatui::Frame<'_>, area: Rect, title: &str) -> Rect {
    frame.render_widget(TuiClear, area);
    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TuiColor::White))
        .style(Style::default().fg(TuiColor::White).bg(TuiColor::Black));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn render_add_account_dialog(stdout: &mut std::io::Stdout, selected: usize) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 76, 8);
        let inner = render_popup_frame(frame, area, " Add account ");
        frame.render_widget(
            Paragraph::new("Choose how to add the account.").wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );
        let options = ["1 OAuth", "2 Device auth", "3 JSON file"];
        let choices = options
            .iter()
            .enumerate()
            .map(|(index, label)| {
                if selected == index {
                    format!("[ {label} ]")
                } else {
                    format!("  {label}  ")
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        frame.render_widget(
            Paragraph::new(choices).alignment(Alignment::Center).style(
                Style::default()
                    .fg(TuiColor::Green)
                    .add_modifier(TuiModifier::BOLD),
            ),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(3),
                width: inner.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new("Enter confirms, Esc cancels.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(TuiColor::Gray)),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(5),
                width: inner.width,
                height: 1,
            },
        );
    })?;
    Ok(())
}

fn render_first_run_dialog(stdout: &mut std::io::Stdout, selected: usize) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 76, 8);
        let inner = render_popup_frame(frame, area, " Get started ");
        frame.render_widget(
            Paragraph::new("No local accounts found. Add one or restore accounts from WebDAV.")
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );
        let options = ["1 Add account", "2 WebDAV sync"];
        let choices = options
            .iter()
            .enumerate()
            .map(|(index, label)| {
                if selected == index {
                    format!("[ {label} ]")
                } else {
                    format!("  {label}  ")
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        frame.render_widget(
            Paragraph::new(choices).alignment(Alignment::Center).style(
                Style::default()
                    .fg(TuiColor::Green)
                    .add_modifier(TuiModifier::BOLD),
            ),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(3),
                width: inner.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new("Enter confirms, Esc cancels.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(TuiColor::Gray)),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(5),
                width: inner.width,
                height: 1,
            },
        );
    })?;
    Ok(())
}

fn import_json_accounts(data: &str, alias: Option<&str>) -> Result<Vec<ImportedAccount>> {
    let root: Value = serde_json::from_str(data).context("failed to parse account JSON")?;
    let mut imported = Vec::new();
    enumerate_objects(&root, &mut |object| {
        if let Some(account) = imported_account_from_object(object, &root, alias) {
            imported.push(account);
        }
    });
    dedupe_imported_accounts(&mut imported);
    if imported.is_empty() {
        bail!("no importable account tokens found in JSON");
    }
    Ok(imported)
}

fn enumerate_objects<'a>(value: &'a Value, visit: &mut impl FnMut(&'a Map<String, Value>)) {
    match value {
        Value::Object(object) => {
            visit(object);
            for value in object.values() {
                enumerate_objects(value, visit);
            }
        }
        Value::Array(values) => {
            for value in values {
                enumerate_objects(value, visit);
            }
        }
        _ => {}
    }
}

fn imported_account_from_object(
    object: &Map<String, Value>,
    root: &Value,
    alias: Option<&str>,
) -> Option<ImportedAccount> {
    let access_token = string_field(object, "access_token")?.to_string();
    let id_token = string_field(object, "id_token").map(str::to_string);
    let refresh_token = string_field(object, "refresh_token").map(str::to_string);
    let account_id = string_field(object, "account_id")
        .or_else(|| string_field(object, "chatgpt_account_id"))
        .map(str::to_string)
        .or_else(|| {
            jwt_string_claim(
                &access_token,
                &["https://api.openai.com/auth", "chatgpt_account_id"],
            )
        })
        .or_else(|| {
            id_token.as_deref().and_then(|token| {
                jwt_string_claim(
                    token,
                    &["https://api.openai.com/auth", "chatgpt_account_id"],
                )
            })
        })
        .or_else(|| first_string_key(root, &["account_id", "chatgpt_account_id"]));
    let email = string_field(object, "email")
        .map(str::to_string)
        .or_else(|| jwt_string_claim(&access_token, &["https://api.openai.com/profile", "email"]))
        .or_else(|| jwt_string_claim(&access_token, &["email"]))
        .or_else(|| {
            id_token
                .as_deref()
                .and_then(|token| jwt_string_claim(token, &["email"]))
        })
        .or_else(|| first_string_key(root, &["email"]));
    let account_name = alias
        .map(str::to_string)
        .or_else(|| string_field(object, "name").map(str::to_string))
        .or_else(|| first_string_key(root, &["name"]));
    let plan = string_field(object, "plan")
        .or_else(|| string_field(object, "plan_type"))
        .map(str::to_string)
        .or_else(|| {
            jwt_string_claim(
                &access_token,
                &["https://api.openai.com/auth", "chatgpt_plan_type"],
            )
        })
        .or_else(|| {
            id_token.as_deref().and_then(|token| {
                jwt_string_claim(token, &["https://api.openai.com/auth", "chatgpt_plan_type"])
            })
        });
    let account_key = account_id
        .clone()
        .or_else(|| email.clone())
        .unwrap_or_else(|| format!("token-{}", &sha256_hex(access_token.as_bytes())[..16]));

    let mut tokens = Map::new();
    tokens.insert("access_token".to_string(), Value::String(access_token));
    if let Some(account_id) = account_id {
        tokens.insert("account_id".to_string(), Value::String(account_id));
    }
    if let Some(id_token) = id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token));
    }
    if let Some(refresh_token) = refresh_token {
        tokens.insert("refresh_token".to_string(), Value::String(refresh_token));
    }
    Some(ImportedAccount {
        auth: json!({
            "auth_mode": "chatgpt",
            "tokens": Value::Object(tokens),
            "last_refresh": chrono::Utc::now().to_rfc3339(),
        }),
        account_key,
        email: email.unwrap_or_default(),
        account_name,
        plan,
    })
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn first_string_key(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = string_field(object, key) {
                    return Some(value.to_string());
                }
            }
            object
                .values()
                .find_map(|value| first_string_key(value, keys))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| first_string_key(value, keys)),
        _ => None,
    }
}

fn jwt_string_claim(token: &str, path: &[&str]) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .ok()?;
    let value: Value = serde_json::from_slice(&data).ok()?;
    let mut current = &value;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn dedupe_imported_accounts(accounts: &mut Vec<ImportedAccount>) {
    let mut seen = BTreeSet::new();
    accounts.retain(|account| seen.insert(account.account_key.clone()));
}

fn persist_imported_accounts(accounts: Vec<ImportedAccount>) -> Result<()> {
    let mut registry = read_registry_or_default()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut last_account_key = None;
    let mut last_snapshot_path = None;
    for imported in accounts {
        let snapshot_path = account_auth_path(&imported.account_key);
        write_private_json(&snapshot_path, &imported.auth)?;
        upsert_account_record(&mut registry, &imported, now_ms);
        last_account_key = Some(imported.account_key.clone());
        last_snapshot_path = Some(snapshot_path);
        println!("imported {}", imported_account_label(&imported));
    }
    if let Some(account_key) = last_account_key {
        registry.active_account_key = Some(account_key);
        registry.active_account_activated_at_ms = Some(now_ms);
    }
    if let Some(snapshot_path) = last_snapshot_path {
        backup_auth_if_changed(&snapshot_path)?;
        copy_private_file(&snapshot_path, &auth_path())?;
    }
    write_registry(&registry)?;
    Ok(())
}

fn upsert_account_record(registry: &mut Registry, imported: &ImportedAccount, now_ms: i64) {
    if let Some(account) = registry
        .accounts
        .iter_mut()
        .find(|account| account.account_key == imported.account_key)
    {
        account.email = imported.email.clone();
        account.account_name = imported.account_name.clone();
        account.plan = imported.plan.clone();
        account.last_used_at = Some(now_ms);
        return;
    }
    registry.accounts.push(AccountRecord {
        account_key: imported.account_key.clone(),
        email: imported.email.clone(),
        alias: String::new(),
        account_name: imported.account_name.clone(),
        plan: imported.plan.clone(),
        last_used_at: Some(now_ms),
        last_usage: None,
        last_usage_at: None,
        inactive: false,
        extra: BTreeMap::new(),
    });
}

fn imported_account_label(account: &ImportedAccount) -> String {
    account
        .account_name
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            if account.email.is_empty() {
                None
            } else {
                Some(account.email.clone())
            }
        })
        .unwrap_or_else(|| account.account_key.clone())
}

fn interactive_webdav_sync() -> Result<()> {
    if !has_saved_webdav_config()? && !configure_webdav_first_run()? {
        return Ok(());
    }
    loop {
        let reachable = webdav_connection_status().is_ok();
        let Some(action) = choose_webdav_action(reachable)? else {
            return Ok(());
        };
        match action {
            WebdavAction::Push => {
                cloud_push(true)?;
                println!("WebDAV push completed.");
                wait_for_enter()?;
                return Ok(());
            }
            WebdavAction::Pull => {
                cloud_pull(true)?;
                println!("WebDAV pull completed.");
                wait_for_enter()?;
                return Ok(());
            }
            WebdavAction::Configure => {
                let _ = configure_webdav_first_run()?;
            }
        }
    }
}

fn interactive_app_server_restart_setting() -> Result<()> {
    let mut settings = read_settings()?;
    let mut enabled = settings.restart_app_server_after_switch;
    let mut terminal = TerminalSession::enter()?;
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_app_server_restart_dialog(&mut terminal.stdout, enabled)?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::Char(' ')
            | KeyCode::Char('e')
            | KeyCode::Char('E') => {
                enabled = !enabled;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                enabled = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                enabled = false;
            }
            KeyCode::Enter => {
                settings.restart_app_server_after_switch = enabled;
                write_settings(&settings)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn render_app_server_restart_dialog(stdout: &mut std::io::Stdout, enabled: bool) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 78, 8);
        let inner = render_popup_frame(frame, area, " Switch behavior ");
        let state = if enabled {
            "After switching, authswap restarts codex app-server with pkill."
        } else {
            "After switching, authswap does not restart codex app-server."
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Restart codex app-server after account switch?\n{state}"
            ))
            .wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 3,
            },
        );
        let on = if enabled { "[ On ]" } else { "  On  " };
        let off = if enabled { "  Off  " } else { "[ Off ]" };
        frame.render_widget(
            Paragraph::new(format!("{on}    {off}"))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(TuiColor::Green)
                        .add_modifier(TuiModifier::BOLD),
                ),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(4),
                width: inner.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new("Enter saves, Space toggles, Esc cancels.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(TuiColor::Gray)),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(6),
                width: inner.width,
                height: 1,
            },
        );
    })?;
    Ok(())
}

fn has_saved_webdav_config() -> Result<bool> {
    let settings = read_settings()?;
    Ok(!settings.webdav.url.trim().is_empty()
        || env::var("AUTHSWAP_WEBDAV_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some())
}

fn configure_webdav_first_run() -> Result<bool> {
    let mut settings = read_settings()?;
    let Some(webdav) = interactive_webdav_config(settings.webdav.clone())? else {
        return Ok(false);
    };
    settings.webdav = webdav;
    write_settings(&settings)?;
    Ok(true)
}

fn interactive_webdav_config(initial: WebdavSettings) -> Result<Option<WebdavSettings>> {
    let mut terminal = TerminalSession::enter()?;
    let mut fields = [initial.url, initial.username, initial.password];
    let mut selected = 0usize;
    let mut status_message: Option<String> = None;
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_webdav_config_dialog(
            &mut terminal.stdout,
            &fields,
            selected,
            status_message.as_deref(),
        )?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                status_message = None;
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1).min(fields.len() - 1);
                status_message = None;
            }
            KeyCode::BackTab => {
                selected = selected.saturating_sub(1);
                status_message = None;
            }
            KeyCode::Backspace => {
                fields[selected].pop();
                status_message = None;
            }
            KeyCode::Delete => {
                fields[selected].clear();
                status_message = None;
            }
            KeyCode::Enter => {
                if fields[0].trim().is_empty() {
                    selected = 0;
                    status_message = Some("WebDAV URL is required.".to_string());
                    continue;
                }
                let webdav = WebdavSettings {
                    url: normalize_webdav_url(&fields[0]),
                    username: fields[1].trim().to_string(),
                    password: fields[2].trim().to_string(),
                };
                match webdav_config_from_settings(&webdav).and_then(|config| {
                    check_webdav_config(&config).context("WebDAV connectivity test failed")
                }) {
                    Ok(()) => return Ok(Some(webdav)),
                    Err(error) => status_message = Some(error.to_string()),
                }
            }
            KeyCode::Char(ch) => {
                fields[selected].push(ch);
                status_message = None;
            }
            _ => {}
        }
    }
}

fn webdav_config_from_settings(settings: &WebdavSettings) -> Result<WebdavConfig> {
    if settings.url.trim().is_empty() {
        bail!("WebDAV URL is required");
    }
    let base_url = normalize_webdav_url(&settings.url);
    Ok(WebdavConfig {
        base_url,
        username: non_empty_string(settings.username.clone()),
        password: non_empty_string(settings.password.clone()),
        token: None,
    })
}

fn render_webdav_config_dialog(
    stdout: &mut std::io::Stdout,
    fields: &[String; 3],
    selected: usize,
    status_message: Option<&str>,
) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 88, 16);
        let inner = render_popup_frame(frame, area, " WebDAV settings ");
        frame.render_widget(
            Paragraph::new("Configure WebDAV sync. URL gets https:// if no scheme is set.")
                .wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );

        let labels = ["URL", "Username", "Password"];
        for (index, label) in labels.iter().enumerate() {
            let value = if index == 2 && !fields[index].is_empty() {
                "*".repeat(fields[index].chars().count())
            } else {
                fields[index].clone()
            };
            let style = if selected == index {
                Style::default()
                    .fg(TuiColor::Green)
                    .bg(TuiColor::DarkGray)
                    .add_modifier(TuiModifier::BOLD)
            } else {
                Style::default().fg(TuiColor::White)
            };
            frame.render_widget(
                Paragraph::new(format!("{label}: {value}"))
                    .style(style)
                    .block(Block::default().borders(Borders::ALL)),
                Rect {
                    x: inner.x,
                    y: inner.y.saturating_add(3 + index as u16 * 3),
                    width: inner.width,
                    height: 3,
                },
            );
        }

        let footer = status_message
            .unwrap_or("Enter tests and saves, Tab moves, Delete clears, Esc cancels.");
        let footer_style = if status_message.is_some() {
            Style::default()
                .fg(TuiColor::Red)
                .add_modifier(TuiModifier::BOLD)
        } else {
            Style::default().fg(TuiColor::Gray)
        };
        frame.render_widget(
            Paragraph::new(footer)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(footer_style),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(inner.height.saturating_sub(2)),
                width: inner.width,
                height: 2,
            },
        );
    })?;
    Ok(())
}

fn normalize_webdav_url(input: &str) -> String {
    let trimmed = input.trim();
    let mut url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

fn prompt_required(message: &str) -> Result<String> {
    loop {
        let value = prompt(message)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }
        println!("Value is required.");
    }
}

fn wait_for_enter() -> Result<()> {
    let _ = prompt("Press Enter to continue...")?;
    Ok(())
}

fn webdav_connection_status() -> Result<()> {
    let config = load_webdav_config()?;
    check_webdav_config(&config)
}

fn check_webdav_config(config: &WebdavConfig) -> Result<()> {
    let client = webdav_client()?;
    let (status, body) = webdav_request(
        &client,
        config,
        Method::from_bytes(b"MKCOL")?,
        "",
        None,
        None,
    )?;
    if matches!(status, 200 | 201 | 301 | 302 | 405) {
        return Ok(());
    }
    bail!(
        "WebDAV check failed with HTTP {status}: {}",
        String::from_utf8_lossy(&body)
    )
}

fn choose_webdav_action(reachable: bool) -> Result<Option<WebdavAction>> {
    let mut terminal = TerminalSession::enter()?;
    let mut selected = 0usize;
    queue!(terminal.stdout, Clear(ClearType::All))?;
    terminal.stdout.flush()?;
    loop {
        render_webdav_dialog(&mut terminal.stdout, selected, reachable)?;
        terminal.stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(None),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                selected = (selected + 1).min(2);
            }
            KeyCode::Char('1') | KeyCode::Char('p') | KeyCode::Char('P') => {
                return Ok(Some(WebdavAction::Push));
            }
            KeyCode::Char('2') | KeyCode::Char('g') | KeyCode::Char('G') => {
                return Ok(Some(WebdavAction::Pull));
            }
            KeyCode::Char('3') | KeyCode::Char('c') | KeyCode::Char('C') => {
                return Ok(Some(WebdavAction::Configure));
            }
            KeyCode::Enter => {
                return Ok(Some(match selected {
                    0 => WebdavAction::Push,
                    1 => WebdavAction::Pull,
                    _ => WebdavAction::Configure,
                }));
            }
            _ => {}
        }
    }
}

fn render_webdav_dialog(
    stdout: &mut std::io::Stdout,
    selected: usize,
    reachable: bool,
) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        render_webdav_dialog_frame(frame, selected, reachable);
    })?;
    Ok(())
}

fn render_webdav_dialog_overlay(
    stdout: &mut std::io::Stdout,
    registry: &Registry,
    selected_account: usize,
    selected: usize,
    reachable: bool,
) -> Result<()> {
    let rows = account_display_rows(registry)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        render_account_picker_frame(frame, &rows, selected_account, "", None);
        render_webdav_dialog_frame(frame, selected, reachable);
    })?;
    Ok(())
}

fn render_webdav_dialog_frame(frame: &mut ratatui::Frame<'_>, selected: usize, reachable: bool) {
    let area = centered_popup_rect(frame.area(), 82, 9);
    let inner = render_popup_frame(frame, area, " WebDAV sync ");
    let status_text = if reachable {
        "● WebDAV reachable"
    } else {
        "● WebDAV unreachable"
    };
    let status_color = if reachable {
        TuiColor::Green
    } else {
        TuiColor::Red
    };
    frame.render_widget(
        Paragraph::new(status_text)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(status_color)
                    .add_modifier(TuiModifier::BOLD),
            ),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );
    let options = ["1 Push overwrite", "2 Pull from WebDAV", "3 Configure"];
    let choices = options
        .iter()
        .enumerate()
        .map(|(index, label)| {
            if selected == index {
                format!("[ {label} ]")
            } else {
                format!("  {label}  ")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    frame.render_widget(
        Paragraph::new(choices).alignment(Alignment::Center).style(
            Style::default()
                .fg(TuiColor::Green)
                .add_modifier(TuiModifier::BOLD),
        ),
        Rect {
            x: inner.x,
            y: inner.y.saturating_add(2),
            width: inner.width,
            height: 1,
        },
    );
    frame.render_widget(
        Paragraph::new(
            "Push overwrites remote files. Pull overwrites local files.\nEnter confirms, Esc cancels.",
        )
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(TuiColor::Gray)),
        Rect {
            x: inner.x,
            y: inner.y.saturating_add(4),
            width: inner.width,
            height: 3,
        },
    );
}

fn confirm_delete_account(stdout: &mut std::io::Stdout, account_label: &str) -> Result<bool> {
    let mut confirm = false;
    queue!(stdout, Clear(ClearType::All))?;
    stdout.flush()?;
    loop {
        render_delete_confirmation_dialog(stdout, account_label, confirm)?;
        stdout.flush()?;
        let Event::Key(key) = read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                confirm = !confirm;
            }
            KeyCode::Enter => return Ok(confirm),
            _ => {}
        }
    }
}

fn render_delete_confirmation_dialog(
    stdout: &mut std::io::Stdout,
    account_label: &str,
    confirm: bool,
) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        let area = centered_popup_rect(frame.area(), 72, 7);
        let inner = render_popup_frame(frame, area, " Delete account? ");
        frame.render_widget(
            Paragraph::new(truncate_display(account_label, inner.width as usize))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );
        let yes = if confirm { "[ Yes ]" } else { "  Yes  " };
        let no = if confirm { "  No  " } else { "[ No ]" };
        frame.render_widget(
            Paragraph::new(format!("{yes}    {no}"))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(TuiColor::Red)
                        .add_modifier(TuiModifier::BOLD),
                ),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(3),
                width: inner.width,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new("Enter confirms, Esc cancels.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(TuiColor::Gray)),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(5),
                width: inner.width,
                height: 1,
            },
        );
    })?;
    Ok(())
}

struct TerminalSession {
    stdout: std::io::Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let mut stdout = std::io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, DisableLineWrap, Hide)?;
        Ok(Self { stdout })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, EnableLineWrap, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn render_account_picker(
    stdout: &mut std::io::Stdout,
    rows: &[AccountDisplayRow],
    selected: usize,
    number_buffer: &str,
    footer: Option<&str>,
) -> Result<()> {
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend)?;
    terminal.draw(|frame| {
        render_account_picker_frame(frame, rows, selected, number_buffer, footer);
    })?;
    Ok(())
}

fn render_account_picker_frame(
    frame: &mut ratatui::Frame<'_>,
    rows: &[AccountDisplayRow],
    selected: usize,
    number_buffer: &str,
    footer: Option<&str>,
) {
    let area = frame.area();
    frame.render_widget(TuiClear, area);

    let terminal_width = area.width as usize;
    let idx_width = digit_width(rows.len()).max(2);
    let widths = table_widths(rows, terminal_width, idx_width);
    let title = Paragraph::new("Select account to activate:").style(
        Style::default()
            .fg(TuiColor::White)
            .add_modifier(TuiModifier::BOLD),
    );
    frame.render_widget(
        title,
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("ID"),
        Cell::from("ACCOUNT"),
        Cell::from("PLAN"),
        Cell::from("5H LIMIT"),
        Cell::from("WEEK LIMIT"),
    ])
    .style(
        Style::default()
            .fg(TuiColor::Gray)
            .add_modifier(TuiModifier::BOLD),
    );

    let table_rows = rows.iter().map(|row| {
        let marker = if row.active { "*" } else { "" };
        let style = if row.index == selected {
            let foreground = if row.inactive {
                TuiColor::Red
            } else {
                TuiColor::Green
            };
            Style::default()
                .fg(foreground)
                .bg(TuiColor::DarkGray)
                .add_modifier(TuiModifier::BOLD)
        } else if row.inactive {
            Style::default().fg(TuiColor::Red)
        } else if row.active {
            Style::default().fg(TuiColor::Green)
        } else {
            Style::default().fg(TuiColor::DarkGray)
        };
        Row::new(vec![
            Cell::from(marker.to_string()),
            Cell::from(format!("{}", row.index + 1)),
            Cell::from(truncate_display(
                &row.account,
                widths
                    .account
                    .saturating_add(widths.email)
                    .saturating_add(2),
            )),
            Cell::from(truncate_display(&row.plan, widths.plan)),
            Cell::from(truncate_display(&row.limit_5h, widths.limit_5h)),
            Cell::from(truncate_display(&row.limit_week, widths.limit_week)),
        ])
        .style(style)
    });

    let table_height = rows
        .len()
        .saturating_add(2)
        .min(area.height.saturating_sub(5) as usize);
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(1),
            Constraint::Length(idx_width as u16),
            Constraint::Length(
                widths
                    .account
                    .saturating_add(widths.email)
                    .saturating_add(2) as u16,
            ),
            Constraint::Length(widths.plan as u16),
            Constraint::Length(widths.limit_5h as u16),
            Constraint::Length(widths.limit_week as u16),
        ],
    )
    .header(header)
    .column_spacing(2);
    frame.render_widget(
        table,
        Rect {
            x: area.x,
            y: area.y.saturating_add(2),
            width: area.width,
            height: table_height as u16,
        },
    );

    let mut help =
        "Keys: Up/Down or j/k move, Enter select, number jump, a add, s settings, w webdav, r refresh, t refresh all, Backspace delete, Esc or q quit"
            .to_string();
    if !number_buffer.is_empty() {
        help.push_str("  typed: ");
        help.push_str(number_buffer);
    }
    let mut footer_lines = vec![help];
    if let Some(message) = footer {
        footer_lines.push(message.to_string());
    }
    let footer_text = footer_lines.join("\n");
    let footer_style = if footer.is_some() {
        Style::default()
            .fg(TuiColor::Red)
            .add_modifier(TuiModifier::BOLD)
    } else {
        Style::default().fg(TuiColor::Gray)
    };
    let footer = Paragraph::new(footer_text)
        .wrap(Wrap { trim: true })
        .style(footer_style);
    let footer_y = area
        .y
        .saturating_add(2)
        .saturating_add(table_height as u16)
        .saturating_add(1);
    frame.render_widget(
        footer,
        Rect {
            x: area.x,
            y: footer_y,
            width: area.width,
            height: area.height.saturating_sub(footer_y.saturating_sub(area.y)),
        },
    );
}

fn picker_prefix(idx_width: usize, selected: bool, active: bool, index: Option<usize>) -> String {
    let cursor = if selected { ">" } else { " " };
    let active = if active { "*" } else { " " };
    match index {
        Some(index) => format!("{cursor}{active} {index:>idx_width$}. "),
        None => format!("{cursor}{active} {:>idx_width$}  ", "idx"),
    }
}

fn picker_line(prefix: &str, cells: PickerCells<'_>, widths: AccountTableWidths) -> String {
    let mut line = String::new();
    line.push_str(prefix);
    push_table_cell(&mut line, cells.account, widths.account);
    line.push_str("  ");
    push_table_cell(&mut line, cells.email, widths.email);
    line.push_str("  ");
    push_table_cell(&mut line, cells.plan, widths.plan);
    line.push_str("  ");
    push_table_cell(&mut line, cells.limit_5h, widths.limit_5h);
    line.push_str("  ");
    push_table_cell(&mut line, cells.limit_week, widths.limit_week);
    line
}

fn push_table_cell(line: &mut String, value: &str, width: usize) {
    let truncated = truncate_display(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(truncated.as_str()));
    line.push_str(&truncated);
    line.push_str(&" ".repeat(padding));
}

fn account_display_rows(registry: &Registry) -> Result<Vec<AccountDisplayRow>> {
    let now = chrono::Utc::now().timestamp();
    Ok(registry
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let active =
                registry.active_account_key.as_deref() == Some(account.account_key.as_str());
            AccountDisplayRow {
                index,
                active,
                inactive: account.inactive,
                account: account.account_name.clone().unwrap_or_else(|| {
                    if !account.email.is_empty() {
                        account.email.clone()
                    } else {
                        account.account_key.clone()
                    }
                }),
                email: dash_if_empty(&account.email),
                plan: display_plan(account),
                limit_5h: usage_limit_text(&account.last_usage, 300, true, now),
                limit_week: usage_limit_text(&account.last_usage, 10080, false, now),
            }
        })
        .collect())
}

fn active_account_index(registry: &Registry) -> Option<usize> {
    let active_key = registry.active_account_key.as_deref()?;
    registry
        .accounts
        .iter()
        .position(|account| account.account_key == active_key)
}

fn dash_if_empty(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

fn display_plan(account: &AccountRecord) -> String {
    account
        .last_usage
        .as_ref()
        .and_then(|usage| usage.plan_type.clone())
        .or_else(|| account.plan.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn usage_limit_text(
    usage: &Option<RateLimitSnapshot>,
    minutes: i64,
    fallback_primary: bool,
    now_seconds: i64,
) -> String {
    let Some(window) = resolve_rate_window(usage.as_ref(), minutes, fallback_primary) else {
        return "-".to_string();
    };
    let percent = format_percent(100.0 - window.used_percent);
    if let Some(reset_at) = window.resets_at {
        let remaining = format_remaining_time(reset_at.saturating_sub(now_seconds));
        return format!("{percent} {remaining}");
    }
    percent
}

fn format_remaining_time(seconds: i64) -> String {
    if seconds <= 0 {
        return "0m".to_string();
    }
    let total_minutes = (seconds + 59) / 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn resolve_rate_window(
    usage: Option<&RateLimitSnapshot>,
    minutes: i64,
    fallback_primary: bool,
) -> Option<&RateLimitWindow> {
    let usage = usage?;
    if let Some(primary) = usage.primary.as_ref() {
        if primary.window_minutes == Some(minutes) {
            return Some(primary);
        }
    }
    if let Some(secondary) = usage.secondary.as_ref() {
        if secondary.window_minutes == Some(minutes) {
            return Some(secondary);
        }
    }
    if fallback_primary {
        usage.primary.as_ref()
    } else {
        usage.secondary.as_ref()
    }
}

fn format_percent(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    if (value.fract()).abs() < 0.05 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn digit_width(value: usize) -> usize {
    value.max(1).to_string().len()
}

fn table_widths(
    rows: &[AccountDisplayRow],
    terminal_width: usize,
    idx_width: usize,
) -> AccountTableWidths {
    let prefix_width = idx_width + 5;
    let separator_width = 10;
    let available = terminal_width.saturating_sub(prefix_width + separator_width);
    let mut widths = AccountTableWidths {
        account: bounded_column_width(
            rows.iter().map(|row| row.account.as_str()),
            "ACCOUNT",
            18,
            30,
        ),
        email: bounded_column_width(rows.iter().map(|row| row.email.as_str()), "EMAIL", 20, 34),
        plan: bounded_column_width(rows.iter().map(|row| row.plan.as_str()), "PLAN", 8, 14),
        limit_5h: bounded_column_width(
            rows.iter().map(|row| row.limit_5h.as_str()),
            "5H LIMIT",
            12,
            16,
        ),
        limit_week: bounded_column_width(
            rows.iter().map(|row| row.limit_week.as_str()),
            "WEEK LIMIT",
            12,
            16,
        ),
    };
    let min = AccountTableWidths {
        account: 7,
        email: 7,
        plan: 4,
        limit_5h: 12,
        limit_week: 12,
    };
    while widths.total() > available {
        if widths.email > min.email && widths.email >= widths.account {
            widths.email -= 1;
        } else if widths.account > min.account {
            widths.account -= 1;
        } else if widths.plan > min.plan {
            widths.plan -= 1;
        } else if widths.limit_week > min.limit_week {
            widths.limit_week -= 1;
        } else if widths.limit_5h > min.limit_5h {
            widths.limit_5h -= 1;
        } else {
            break;
        }
    }
    widths
}

impl AccountTableWidths {
    fn total(self) -> usize {
        self.account + self.email + self.plan + self.limit_5h + self.limit_week
    }
}

fn bounded_column_width<'a>(
    values: impl Iterator<Item = &'a str>,
    header: &str,
    min: usize,
    max: usize,
) -> usize {
    values
        .map(UnicodeWidthStr::width)
        .chain(std::iter::once(UnicodeWidthStr::width(header)))
        .max()
        .unwrap_or(min)
        .clamp(min, max)
}

fn truncate_display(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let suffix = "...";
    let target = width - UnicodeWidthStr::width(suffix);
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > target {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push_str(suffix);
    out
}

fn wrap_display(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for raw_line in value.lines() {
        let mut line = String::new();
        let mut used = 0usize;
        for word in raw_line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let separator_width = usize::from(!line.is_empty());
            if !line.is_empty() && used + separator_width + word_width > width {
                lines.push(line);
                line = String::new();
                used = 0;
            }
            if word_width > width {
                if !line.is_empty() {
                    lines.push(line);
                    line = String::new();
                    used = 0;
                }
                for chunk in wrap_long_display_word(word, width) {
                    lines.push(chunk);
                }
                continue;
            }
            if !line.is_empty() {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += word_width;
        }
        if line.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_long_display_word(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used > 0 && used + ch_width > width {
            lines.push(line);
            line = String::new();
            used = 0;
        }
        line.push(ch);
        used += ch_width;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn switch_account(query: Option<String>) -> Result<()> {
    let mut registry = read_registry()?;
    sync_active_auth_snapshot(&registry)?;
    let target = resolve_account(&registry, query.as_deref())?;
    let source = account_auth_path(&target.account_key);
    if !source.is_file() {
        bail!("auth file not found: {}", source.display());
    }
    backup_auth_if_changed(&source)?;
    copy_private_file(&source, &auth_path())?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let previous_active = registry.active_account_key.clone();
    registry.active_account_key = Some(target.account_key.clone());
    registry.active_account_activated_at_ms = Some(now_ms);
    if previous_active.as_deref() != Some(target.account_key.as_str()) {
        if let Some(previous_active) = previous_active {
            registry.extra.insert(
                "previous_active_account_key".to_string(),
                Value::String(previous_active),
            );
        }
    }
    for account in &mut registry.accounts {
        if account.account_key == target.account_key {
            account.last_used_at = Some(now_ms);
        }
    }
    write_registry(&registry)?;
    restart_app_server_after_switch_if_enabled()?;
    println!("switched to {}", display_account(&target));
    Ok(())
}

fn restart_app_server_after_switch_if_enabled() -> Result<()> {
    if !read_settings()?.restart_app_server_after_switch {
        return Ok(());
    }
    let Ok(user) = env::var("USER") else {
        return Ok(());
    };
    let _ = Command::new("pkill")
        .arg("-u")
        .arg(user)
        .arg("-f")
        .arg("codex app-server")
        .status();
    Ok(())
}

fn resolve_account(registry: &Registry, query: Option<&str>) -> Result<AccountRecord> {
    let query = query.unwrap_or("").trim();
    if query.is_empty() {
        if let Some(key) = registry.active_account_key.as_deref() {
            if let Some(account) = registry.accounts.iter().find(|a| a.account_key == key) {
                return Ok(account.clone());
            }
        }
        return registry
            .accounts
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no accounts found"));
    }
    let query_lower = query.to_ascii_lowercase();
    let matches: Vec<_> = registry
        .accounts
        .iter()
        .filter(|account| {
            account.account_key == query
                || account.alias.eq_ignore_ascii_case(query)
                || account.email.eq_ignore_ascii_case(query)
                || account.email.to_ascii_lowercase().contains(&query_lower)
                || account.alias.to_ascii_lowercase().contains(&query_lower)
        })
        .cloned()
        .collect();
    match matches.len() {
        0 => bail!("no account matched `{query}`"),
        1 => Ok(matches[0].clone()),
        _ => bail!("multiple accounts matched `{query}`; use a more specific query"),
    }
}

fn delete_account_at(registry: &mut Registry, index: usize) -> Result<()> {
    if index >= registry.accounts.len() {
        bail!("selection out of range: {}", index + 1);
    }
    let account = registry.accounts.remove(index);
    let snapshot_path = account_auth_path(&account.account_key);
    if snapshot_path.exists() {
        fs::remove_file(&snapshot_path)
            .with_context(|| format!("failed to delete {}", snapshot_path.display()))?;
    }
    if registry.active_account_key.as_deref() == Some(account.account_key.as_str()) {
        registry.active_account_key = None;
        registry.active_account_activated_at_ms = None;
        let current_auth_path = auth_path();
        if current_auth_path.exists() {
            fs::remove_file(&current_auth_path)
                .with_context(|| format!("failed to delete {}", current_auth_path.display()))?;
        }
    }
    if registry
        .extra
        .get("previous_active_account_key")
        .and_then(Value::as_str)
        == Some(account.account_key.as_str())
    {
        registry.extra.remove("previous_active_account_key");
    }
    write_registry(registry)?;
    Ok(())
}

fn refresh_account_usage_at(registry: &mut Registry, index: usize) -> Result<()> {
    if index >= registry.accounts.len() {
        bail!("selection out of range: {}", index + 1);
    }
    let account_key = registry.accounts[index].account_key.clone();
    let result = refresh_account_usage_cache(registry, &account_key);
    write_registry(registry)?;
    result
}

fn refresh_all_account_usage(registry: &mut Registry) -> Result<String> {
    let account_keys = registry
        .accounts
        .iter()
        .map(|account| account.account_key.clone())
        .collect::<Vec<_>>();
    let total = account_keys.len();
    let mut refreshed = 0usize;
    let mut failures = Vec::new();

    for (index, account_key) in account_keys.iter().enumerate() {
        match refresh_account_usage_cache(registry, account_key) {
            Ok(()) => refreshed += 1,
            Err(error) => failures.push(format!("{account_key}: {error:#}")),
        }
        if index + 1 < total {
            thread::sleep(Duration::from_millis(100));
        }
    }
    write_registry(registry)?;

    if failures.is_empty() {
        Ok(format!("Usage refreshed for all {refreshed} accounts."))
    } else {
        Ok(format!(
            "Usage refreshed for {refreshed}/{total} accounts; {} failed: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

fn refresh_account_usage_cache(registry: &mut Registry, account_key: &str) -> Result<()> {
    let usage = match fetch_account_usage(account_key) {
        Ok(usage) => usage,
        Err(CodexUsageError::Unauthorized(message)) => {
            mark_account_inactive(registry, account_key, true)?;
            bail!("{message}");
        }
        Err(CodexUsageError::Other(error)) => return Err(error),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let Some(account) = registry
        .accounts
        .iter_mut()
        .find(|account| account.account_key == account_key)
    else {
        bail!("account not found: {account_key}");
    };
    if usage.plan_type.is_some() {
        account.plan = usage.plan_type.clone();
    }
    account.inactive = false;
    account.last_usage = Some(rate_limit_snapshot_from_usage(usage));
    account.last_usage_at = Some(now_ms);
    Ok(())
}

fn mark_account_inactive(registry: &mut Registry, account_key: &str, inactive: bool) -> Result<()> {
    let Some(account) = registry
        .accounts
        .iter_mut()
        .find(|account| account.account_key == account_key)
    else {
        bail!("account not found: {account_key}");
    };
    account.inactive = inactive;
    Ok(())
}

fn fetch_account_usage(
    account_key: &str,
) -> std::result::Result<CodexUsageResponse, CodexUsageError> {
    let auth = read_account_auth_snapshot(account_key).map_err(CodexUsageError::Other)?;
    let usage_url = env::var("AUTHSWAP_CODEX_USAGE_URL")
        .unwrap_or_else(|_| DEFAULT_CODEX_USAGE_URL.to_string());
    let client = codex_usage_client()
        .context("failed to build Codex usage client")
        .map_err(CodexUsageError::Other)?;
    let mut request = client
        .get(&usage_url)
        .bearer_auth(&auth.tokens.access_token)
        .header("Accept", "application/json")
        .header("User-Agent", "authswap");
    if let Some(account_id) = auth
        .tokens
        .account_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to request {usage_url}"))
        .map_err(CodexUsageError::Other)?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Codex usage response")
        .map_err(CodexUsageError::Other)?;
    if status.as_u16() == 401 {
        return Err(CodexUsageError::Unauthorized(format!(
            "Codex usage request failed with HTTP {status}: {}",
            truncate_display(body.trim(), 160)
        )));
    }
    if !status.is_success() {
        return Err(CodexUsageError::Other(anyhow!(
            "Codex usage request failed with HTTP {status}: {}",
            truncate_display(body.trim(), 160)
        )));
    }
    serde_json::from_str(&body)
        .context("failed to parse Codex usage response")
        .map_err(CodexUsageError::Other)
}

fn codex_usage_client() -> Result<Client> {
    let builder = Client::builder();
    apply_env_proxy(builder)?
        .build()
        .context("failed to build HTTP client")
}

fn apply_env_proxy(builder: ClientBuilder) -> Result<ClientBuilder> {
    match env_proxy_url() {
        Some(proxy_url) => Ok(builder.proxy(
            Proxy::all(&proxy_url)
                .with_context(|| format!("invalid proxy URL in environment: {proxy_url}"))?,
        )),
        None => Ok(builder),
    }
}

fn env_proxy_url() -> Option<String> {
    let keys = [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    proxy_url_from_pairs(
        keys.iter()
            .filter_map(|key| env::var(key).ok().map(|value| (key.to_string(), value))),
    )
}

fn proxy_url_from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Option<String> {
    let values: BTreeMap<_, _> = pairs.into_iter().collect();
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .filter_map(|key| values.get(*key))
    .map(|value| value.trim().to_string())
    .find(|value| !value.is_empty())
}

fn read_account_auth_snapshot(account_key: &str) -> Result<AuthSnapshot> {
    let path = account_auth_path(account_key);
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

fn rate_limit_snapshot_from_usage(usage: CodexUsageResponse) -> RateLimitSnapshot {
    RateLimitSnapshot {
        primary: Some(rate_limit_window_from_usage(
            usage.rate_limit.primary_window,
        )),
        secondary: Some(rate_limit_window_from_usage(
            usage.rate_limit.secondary_window,
        )),
        plan_type: usage.plan_type,
        extra: BTreeMap::new(),
    }
}

fn rate_limit_window_from_usage(window: CodexUsageWindow) -> RateLimitWindow {
    RateLimitWindow {
        used_percent: window.used_percent,
        window_minutes: Some(window.limit_window_seconds / 60),
        resets_at: Some(window.reset_at),
        extra: BTreeMap::new(),
    }
}

fn display_account(account: &AccountRecord) -> String {
    if !account.alias.is_empty() {
        account.alias.clone()
    } else if !account.email.is_empty() {
        account.email.clone()
    } else {
        account.account_key.clone()
    }
}

fn account_auth_path(account_key: &str) -> PathBuf {
    accounts_dir().join(format!("{}.auth.json", account_file_key(account_key)))
}

fn account_file_key(account_key: &str) -> String {
    if account_key
        .bytes()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
    {
        account_key.to_string()
    } else {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(account_key.as_bytes())
    }
}

fn sync_active_auth_snapshot(registry: &Registry) -> Result<()> {
    let Some(active_key) = registry.active_account_key.as_deref() else {
        return Ok(());
    };
    let current_auth_path = auth_path();
    if !current_auth_path.is_file() {
        return Ok(());
    }
    let snapshot_path = account_auth_path(active_key);
    if files_equal(&current_auth_path, &snapshot_path)? {
        return Ok(());
    }
    copy_private_file(&current_auth_path, &snapshot_path)
}

fn reconcile_active_account_from_current_auth(registry: &mut Registry) -> Result<()> {
    let current_auth_path = auth_path();
    if !current_auth_path.is_file() {
        return Ok(());
    }
    let active_key = active_account_key_from_current_auth(registry, &current_auth_path)?;
    let Some(active_key) = active_key else {
        return Ok(());
    };
    if registry.active_account_key.as_deref() == Some(active_key.as_str()) {
        return Ok(());
    }
    registry.active_account_key = Some(active_key);
    registry.active_account_activated_at_ms = Some(chrono::Utc::now().timestamp_millis());
    write_registry(registry)
}

fn active_account_key_from_current_auth(
    registry: &Registry,
    current_auth_path: &Path,
) -> Result<Option<String>> {
    for account in &registry.accounts {
        if files_equal(current_auth_path, &account_auth_path(&account.account_key))? {
            return Ok(Some(account.account_key.clone()));
        }
    }

    let data = fs::read_to_string(current_auth_path)
        .with_context(|| format!("failed to read {}", current_auth_path.display()))?;
    let current_auth: AuthSnapshot = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", current_auth_path.display()))?;
    let current_ids = auth_identity_values(&current_auth);
    if current_ids.is_empty() {
        return Ok(None);
    }

    for account in &registry.accounts {
        if current_ids.contains(&account.account_key)
            || (!account.email.is_empty() && current_ids.contains(&account.email))
        {
            return Ok(Some(account.account_key.clone()));
        }
    }

    for account in &registry.accounts {
        let snapshot_path = account_auth_path(&account.account_key);
        if !snapshot_path.is_file() {
            continue;
        }
        let data = fs::read_to_string(&snapshot_path)
            .with_context(|| format!("failed to read {}", snapshot_path.display()))?;
        let snapshot: AuthSnapshot = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", snapshot_path.display()))?;
        let snapshot_ids = auth_identity_values(&snapshot);
        if snapshot_ids.iter().any(|value| current_ids.contains(value)) {
            return Ok(Some(account.account_key.clone()));
        }
    }

    Ok(None)
}

fn auth_identity_values(auth: &AuthSnapshot) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    if let Some(account_id) = auth
        .tokens
        .account_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        values.insert(account_id.to_string());
    }
    if let Some(account_id) = jwt_string_claim(
        &auth.tokens.access_token,
        &["https://api.openai.com/auth", "chatgpt_account_id"],
    ) {
        values.insert(account_id);
    }
    if let Some(email) = jwt_string_claim(
        &auth.tokens.access_token,
        &["https://api.openai.com/profile", "email"],
    )
    .or_else(|| jwt_string_claim(&auth.tokens.access_token, &["email"]))
    {
        values.insert(email);
    }
    if let Some(id_token) = auth.tokens.id_token.as_deref() {
        if let Some(account_id) = jwt_string_claim(
            id_token,
            &["https://api.openai.com/auth", "chatgpt_account_id"],
        ) {
            values.insert(account_id);
        }
        if let Some(email) = jwt_string_claim(id_token, &["email"]) {
            values.insert(email);
        }
    }
    values.insert(format!(
        "token-{}",
        &sha256_hex(auth.tokens.access_token.as_bytes())[..16]
    ));
    values
}

fn backup_auth_if_changed(new_auth_path: &Path) -> Result<()> {
    let current_auth_path = auth_path();
    if !current_auth_path.is_file() || files_equal(&current_auth_path, new_auth_path)? {
        return Ok(());
    }
    fs::create_dir_all(accounts_dir())?;
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    for suffix in 0..100 {
        let name = if suffix == 0 {
            format!("auth.json.bak.{timestamp}")
        } else {
            format!("auth.json.bak.{timestamp}.{suffix}")
        };
        let destination = accounts_dir().join(name);
        if destination.exists() {
            continue;
        }
        copy_private_file(&current_auth_path, &destination)?;
        prune_auth_backups()?;
        return Ok(());
    }
    bail!("unable to allocate auth.json backup name")
}

fn prune_auth_backups() -> Result<()> {
    let dir = accounts_dir();
    if !dir.is_dir() {
        return Ok(());
    }
    let mut backups = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("auth.json.bak.")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|(modified, _)| *modified);
    let remove_count = backups.len().saturating_sub(5);
    for (_, path) in backups.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    if !a.is_file() || !b.is_file() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

fn local_hash_for_manifest_path(destination: &Path, manifest_path: &str) -> Result<String> {
    if manifest_path == "config.toml" {
        return Ok(sha256_hex(
            filter_config_toml_projects(&fs::read_to_string(destination)?)?.as_bytes(),
        ));
    }
    Ok(sha256_hex(&fs::read(destination)?))
}

fn copy_private_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = fs::read(source)?;
    let tmp = destination.with_extension(format!("json.tmp-{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&data)?;
    }
    set_private_file_permissions(&tmp)?;
    fs::rename(&tmp, destination)?;
    set_private_file_permissions(destination)?;
    Ok(())
}

fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    set_private_file_permissions(&tmp)?;
    fs::rename(&tmp, path)?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn status() -> Result<()> {
    let registry = read_registry()?;
    sync_active_auth_snapshot(&registry)?;
    println!("codex_home={}", codex_home().display());
    println!(
        "active={}",
        registry.active_account_key.as_deref().unwrap_or("-")
    );
    println!("accounts={}", registry.accounts.len());
    Ok(())
}

fn settings() -> Result<()> {
    let mut settings = read_settings()?;
    println!("Settings file: {}", settings_path().display());
    println!();
    configure_webdav(&mut settings)?;
    write_settings(&settings)?;
    println!("settings saved");
    Ok(())
}

fn configure_webdav(settings: &mut AppSettings) -> Result<()> {
    println!(
        "WebDAV URL: {}",
        dash_if_empty(settings.webdav.url.as_str())
    );
    let url = prompt("WebDAV URL [Enter to keep, - to clear]: ")?;
    apply_optional_setting(&mut settings.webdav.url, &url);

    println!(
        "WebDAV username: {}",
        dash_if_empty(settings.webdav.username.as_str())
    );
    let username = prompt("WebDAV username [Enter to keep, - to clear]: ")?;
    apply_optional_setting(&mut settings.webdav.username, &username);

    let password_status = if settings.webdav.password.is_empty() {
        "-"
    } else {
        "<configured>"
    };
    println!("WebDAV password: {password_status}");
    let password = prompt("WebDAV password [Enter to keep, - to clear]: ")?;
    apply_optional_setting(&mut settings.webdav.password, &password);
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

fn apply_optional_setting(target: &mut String, value: &str) {
    match value.trim() {
        "" => {}
        "-" => target.clear(),
        _ => *target = value.trim().to_string(),
    }
}

fn load_webdav_config() -> Result<WebdavConfig> {
    let settings = read_settings()?;
    let mut base_url = env::var("AUTHSWAP_WEBDAV_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            if settings.webdav.url.is_empty() {
                None
            } else {
                Some(settings.webdav.url.clone())
            }
        })
        .context("missing AUTHSWAP_WEBDAV_URL or settings webdav url")?;
    if !base_url.ends_with('/') {
        base_url.push('/');
    }
    Ok(WebdavConfig {
        base_url,
        username: env::var("AUTHSWAP_WEBDAV_USERNAME")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| non_empty_string(settings.webdav.username.clone())),
        password: env::var("AUTHSWAP_WEBDAV_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| non_empty_string(settings.webdav.password.clone())),
        token: env::var("AUTHSWAP_WEBDAV_TOKEN").ok(),
    })
}

fn non_empty_string(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn webdav_client() -> Result<Client> {
    Client::builder()
        .build()
        .context("failed to build WebDAV client")
}

fn webdav_request(
    client: &Client,
    config: &WebdavConfig,
    method: Method,
    relative_path: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<(u16, Vec<u8>)> {
    let url = format!("{}{}", config.base_url, encode_path(relative_path));
    let mut request = client.request(method, url);
    if let Some(token) = &config.token {
        request = request.bearer_auth(token);
    } else if config.username.is_some() || config.password.is_some() {
        request = request.basic_auth(
            config.username.clone().unwrap_or_default(),
            Some(config.password.clone().unwrap_or_default()),
        );
    }
    if let Some(content_type) = content_type {
        request = request.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send()?;
    let status = response.status().as_u16();
    let body = response.bytes()?.to_vec();
    Ok((status, body))
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|part| utf8_percent_encode(part, PATH_SEGMENT_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn ensure_remote_dirs(client: &Client, config: &WebdavConfig, relative_path: &str) -> Result<()> {
    let (root_status, root_body) = webdav_request(
        client,
        config,
        Method::from_bytes(b"MKCOL")?,
        "",
        None,
        None,
    )?;
    if !matches!(root_status, 200 | 201 | 301 | 302 | 405) {
        bail!(
            "MKCOL base collection failed with HTTP {root_status}: {}",
            String::from_utf8_lossy(&root_body)
        );
    }

    let mut current = String::new();
    let parts: Vec<_> = relative_path.split('/').collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if current.is_empty() {
            current.push_str(part);
        } else {
            current.push('/');
            current.push_str(part);
        }
        let method = Method::from_bytes(b"MKCOL")?;
        let (status, body) =
            webdav_request(client, config, method, &(current.clone() + "/"), None, None)?;
        if !matches!(status, 201 | 301 | 302 | 405) {
            bail!(
                "MKCOL {current}/ failed with HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
    }
    Ok(())
}

fn local_sync_files() -> Result<Vec<SyncFile>> {
    let home = codex_home();
    let mut files = Vec::new();
    add_if_exists(&mut files, &home, "auth.json");
    add_config_if_exists(&mut files, &home)?;
    add_if_exists(&mut files, &home, "accounts/registry.json");
    let dir = accounts_dir();
    if dir.is_dir() {
        for entry in WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if entry.file_type().is_file() {
                let name = entry.file_name().to_string_lossy();
                if name.ends_with(".auth.json") {
                    add_if_exists(&mut files, &home, &format!("accounts/{name}"));
                }
            }
        }
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

fn add_if_exists(files: &mut Vec<SyncFile>, home: &Path, relative: &str) {
    let path = home.join(relative);
    if path.is_file() {
        if let Ok(data) = fs::read(&path) {
            files.push(SyncFile {
                relative_path: relative.to_string(),
                absolute_path: path,
                data,
            });
        }
    }
}

fn add_config_if_exists(files: &mut Vec<SyncFile>, home: &Path) -> Result<()> {
    let path = config_path();
    if path.is_file() {
        let data = filter_config_toml_projects(&fs::read_to_string(&path)?)?.into_bytes();
        files.push(SyncFile {
            relative_path: "config.toml".to_string(),
            absolute_path: home.join("config.toml"),
            data,
        });
    }
    Ok(())
}

fn filter_config_toml_projects(input: &str) -> Result<String> {
    let mut output = String::new();
    let mut skipping_project_table = false;
    for line in input.lines() {
        if let Some(table_name) = toml_table_name(line) {
            skipping_project_table = is_project_path_table(table_name);
        }
        if !skipping_project_table {
            output.push_str(line);
            output.push('\n');
        }
    }
    Ok(output)
}

fn extract_config_toml_projects(input: &str) -> String {
    let mut output = String::new();
    let mut in_project_table = false;
    for line in input.lines() {
        if let Some(table_name) = toml_table_name(line) {
            in_project_table = is_project_path_table(table_name);
        }
        if in_project_table {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn merge_config_preserving_local_projects(remote: &str, local: Option<&str>) -> Result<String> {
    let mut merged = filter_config_toml_projects(remote)?;
    let local_projects = local.map(extract_config_toml_projects).unwrap_or_default();
    if local_projects.is_empty() {
        return Ok(merged);
    }
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    if !merged.trim().is_empty() {
        merged.push('\n');
    }
    merged.push_str(&local_projects);
    Ok(merged)
}

fn toml_table_name(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') || trimmed.starts_with("[[") {
        return None;
    }
    let end = trimmed.rfind(']')?;
    if end == 0 || trimmed[..end].ends_with(']') {
        return None;
    }
    Some(trimmed[1..end].trim())
}

fn is_project_path_table(table_name: &str) -> bool {
    let Some(rest) = table_name.strip_prefix("projects.") else {
        return false;
    };
    let Some(path) = parse_toml_quoted_prefix(rest) else {
        return false;
    };
    path.starts_with('/') || path.starts_with('~') || looks_like_windows_absolute_path(path)
}

fn parse_toml_quoted_prefix(value: &str) -> Option<&str> {
    let mut chars = value.char_indices();
    let (_, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut escaped = false;
    for (idx, ch) in chars {
        if quote == '"' && escaped {
            escaped = false;
            continue;
        }
        if quote == '"' && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(&value[1..idx]);
        }
    }
    None
}

fn looks_like_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn file_record(file: &SyncFile) -> Result<FileRecord> {
    let metadata = fs::metadata(&file.absolute_path)?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    Ok(FileRecord {
        path: file.relative_path.clone(),
        size: file.data.len() as u64,
        sha256: sha256_hex(&file.data),
        mtime_ms,
    })
}

fn build_manifest() -> Result<Manifest> {
    let files = local_sync_files()?
        .iter()
        .map(file_record)
        .collect::<Result<Vec<_>>>()?;
    Ok(Manifest {
        version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        codex_home: codex_home().display().to_string(),
        files,
    })
}

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn download_manifest(client: &Client, config: &WebdavConfig) -> Result<Option<Manifest>> {
    let (status, body) = webdav_request(client, config, Method::GET, MANIFEST_NAME, None, None)?;
    if matches!(status, 404 | 409) {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        bail!(
            "GET {MANIFEST_NAME} failed with HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    if body.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&body)?))
}

fn verify_remote_file(
    client: &Client,
    config: &WebdavConfig,
    relative_path: &str,
    expected: &[u8],
) -> Result<()> {
    let (status, body) = webdav_request(client, config, Method::GET, relative_path, None, None)?;
    if !(200..300).contains(&status) {
        bail!(
            "remote verification GET {relative_path} failed with HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    let expected_hash = sha256_hex(expected);
    let actual_hash = sha256_hex(&body);
    if actual_hash != expected_hash {
        bail!(
            "remote verification failed for {relative_path}: expected sha256 {expected_hash}, got {actual_hash}. The WebDAV endpoint may not support file PUT."
        );
    }
    Ok(())
}

fn cloud_push(force: bool) -> Result<()> {
    let config = load_webdav_config()?;
    let client = webdav_client()?;
    let files = local_sync_files()?;
    if files.is_empty() {
        bail!("no authswap files found under {}", codex_home().display());
    }
    if let Some(remote_manifest) = download_manifest(&client, &config)? {
        if !force {
            let local_paths: BTreeSet<_> = files
                .iter()
                .map(|file| file.relative_path.clone())
                .collect();
            let remote_paths: BTreeSet<_> = remote_manifest
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect();
            if remote_paths.difference(&local_paths).next().is_some() {
                bail!("remote contains files not present locally. Re-run with --force to overwrite remote manifest.");
            }
        }
    }
    for file in files {
        ensure_remote_dirs(&client, &config, &file.relative_path)?;
        let body = file.data;
        let (status, response_body) = webdav_request(
            &client,
            &config,
            Method::PUT,
            &file.relative_path,
            Some(body.clone()),
            Some("application/json"),
        )?;
        if !(200..300).contains(&status) {
            bail!(
                "PUT {} failed with HTTP {status}: {}",
                file.relative_path,
                String::from_utf8_lossy(&response_body)
            );
        }
        verify_remote_file(&client, &config, &file.relative_path, &body)?;
        println!("pushed {}", file.relative_path);
    }
    let body = serde_json::to_vec_pretty(&build_manifest()?)?;
    let (status, response_body) = webdav_request(
        &client,
        &config,
        Method::PUT,
        MANIFEST_NAME,
        Some(body.clone()),
        Some("application/json"),
    )?;
    if !(200..300).contains(&status) {
        bail!(
            "PUT {MANIFEST_NAME} failed with HTTP {status}: {}",
            String::from_utf8_lossy(&response_body)
        );
    }
    verify_remote_file(&client, &config, MANIFEST_NAME, &body)?;
    println!("pushed {MANIFEST_NAME}");
    Ok(())
}

fn cloud_pull(force: bool) -> Result<()> {
    let config = load_webdav_config()?;
    let client = webdav_client()?;
    let manifest = download_manifest(&client, &config)?
        .ok_or_else(|| anyhow!("remote manifest.json does not exist"))?;
    for file in manifest.files {
        let safe_path = validate_manifest_path(&file.path)?;
        let destination = codex_home().join(safe_path);
        if destination.exists() && !force {
            let current_hash = local_hash_for_manifest_path(&destination, &file.path)?;
            if current_hash != file.sha256 {
                bail!(
                    "local {} differs from remote. Re-run with --force to overwrite it.",
                    file.path
                );
            }
            println!("unchanged {}", file.path);
            continue;
        }
        let (status, body) = webdav_request(&client, &config, Method::GET, &file.path, None, None)?;
        if !(200..300).contains(&status) {
            bail!(
                "GET {} failed with HTTP {status}: {}",
                file.path,
                String::from_utf8_lossy(&body)
            );
        }
        let body = if file.path == "config.toml" {
            let remote = filter_config_toml_projects(&String::from_utf8(body)?)?;
            if sha256_hex(remote.as_bytes()) != file.sha256 {
                bail!("checksum mismatch for {}", file.path);
            }
            let local = if destination.is_file() {
                Some(fs::read_to_string(&destination)?)
            } else {
                None
            };
            merge_config_preserving_local_projects(&remote, local.as_deref())?.into_bytes()
        } else {
            if sha256_hex(&body) != file.sha256 {
                bail!("checksum mismatch for {}", file.path);
            }
            body
        };
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = destination.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, body)?;
        set_private_file_permissions(&tmp)?;
        fs::rename(&tmp, destination)?;
        println!("pulled {}", file.path);
    }
    Ok(())
}

fn validate_manifest_path(path: &str) -> Result<&str> {
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        bail!("remote manifest contains absolute path: {path}");
    }
    for component in parsed.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("remote manifest contains unsafe path: {path}"),
        }
    }
    if !(matches!(path, "auth.json" | "config.toml" | "accounts/registry.json")
        || path.starts_with("accounts/") && path.ends_with(".auth.json"))
    {
        bail!("remote manifest contains unsupported path: {path}");
    }
    Ok(path)
}

fn cloud_status() -> Result<()> {
    let config = load_webdav_config()?;
    let client = webdav_client()?;
    let local = build_manifest()?;
    let Some(remote) = download_manifest(&client, &config)? else {
        println!("remote: missing manifest.json");
        println!("local: {} file(s)", local.files.len());
        return Ok(());
    };
    let local_map: BTreeMap<_, _> = local
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect();
    let remote_map: BTreeMap<_, _> = remote
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect();
    let paths: BTreeSet<_> = local_map.keys().chain(remote_map.keys()).cloned().collect();
    let mut changed = 0usize;
    for path in paths {
        let state = match (local_map.get(&path), remote_map.get(&path)) {
            (None, Some(_)) => "remote-only",
            (Some(_), None) => "local-only",
            (Some(local), Some(remote)) if local.sha256 != remote.sha256 => "different",
            _ => "same",
        };
        if state != "same" {
            changed += 1;
        }
        println!("{state:<12} {path}");
    }
    println!("{changed} changed file(s)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_file_key_keeps_safe_names() {
        assert_eq!(account_file_key("safe-name_1.2"), "safe-name_1.2");
    }

    #[test]
    fn account_file_key_encodes_record_keys() {
        assert_eq!(
            account_file_key("user-a::acct-b"),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("user-a::acct-b")
        );
    }

    #[test]
    fn encode_path_preserves_slashes_and_safe_file_chars() {
        assert_eq!(
            encode_path("accounts/a b.auth.json"),
            "accounts/a%20b.auth.json"
        );
    }

    #[test]
    fn validate_manifest_path_rejects_parent_components() {
        assert!(validate_manifest_path("../auth.json").is_err());
        assert!(validate_manifest_path("accounts/../../auth.json").is_err());
    }

    #[test]
    fn filter_config_toml_removes_path_project_tables() {
        let input = r#"
model = "gpt-5"

[projects."/tmp/a"]
trust_level = "trusted"

[projects."/tmp/a".mcp_servers.foo]
command = "bad"

[profiles.default]
model = "gpt-5"

[projects."relative-name"]
trust_level = "trusted"
"#;
        let filtered = filter_config_toml_projects(input).unwrap();
        assert!(filtered.contains("model = \"gpt-5\""));
        assert!(filtered.contains("[profiles.default]"));
        assert!(filtered.contains("[projects.\"relative-name\"]"));
        assert!(!filtered.contains("[projects.\"/tmp/a\"]"));
        assert!(!filtered.contains("command = \"bad\""));
    }

    #[test]
    fn merge_config_preserves_local_project_trust_only() {
        let remote = r#"model = "gpt-5.1"

[features]
new_ui = true

[projects."/remote/path"]
trust_level = "untrusted"
"#;
        let local = r#"model = "old"

[features]
new_ui = false

[projects."/work/authswap"]
trust_level = "trusted"

[projects."/work/other"]
trust_level = "untrusted"
"#;
        let merged = merge_config_preserving_local_projects(remote, Some(local)).unwrap();
        assert!(merged.contains("model = \"gpt-5.1\""));
        assert!(merged.contains("new_ui = true"));
        assert!(!merged.contains("model = \"old\""));
        assert!(!merged.contains("new_ui = false"));
        assert!(!merged.contains("/remote/path"));
        assert!(merged.contains("[projects.\"/work/authswap\"]"));
        assert!(merged.contains("[projects.\"/work/other\"]"));
    }

    #[test]
    fn validate_manifest_path_allows_config_toml() {
        assert!(validate_manifest_path("config.toml").is_ok());
    }

    #[test]
    fn truncate_display_shortens_long_ascii_values_to_exact_width() {
        let value = truncate_display("very-long-account-name-that-will-not-fit", 18);
        assert_eq!(UnicodeWidthStr::width(value.as_str()), 18);
        assert!(value.ends_with("..."));
    }

    #[test]
    fn truncate_display_respects_unicode_display_width() {
        let value = truncate_display("中文账号名字非常非常长", 12);
        assert_eq!(UnicodeWidthStr::width(value.as_str()), 11);
        assert!(value.ends_with("..."));
        let padding = 12usize.saturating_sub(UnicodeWidthStr::width(value.as_str()));
        assert_eq!(padding, 1);
    }

    #[test]
    fn picker_line_can_be_cropped_to_terminal_width() {
        let rows = vec![AccountDisplayRow {
            index: 0,
            active: true,
            account: "lec+applecoastriver-with-a-very-long-name".to_string(),
            email: "seansanilec+applecoastriver@gmail.example".to_string(),
            plan: "plus".to_string(),
            inactive: false,
            limit_5h: "22%".to_string(),
            limit_week: "55%".to_string(),
        }];
        let terminal_width = 72;
        let idx_width = 2;
        let widths = table_widths(&rows, terminal_width, idx_width);
        let line = picker_line(
            &picker_prefix(idx_width, true, true, Some(1)),
            PickerCells {
                account: &rows[0].account,
                email: &rows[0].email,
                plan: &rows[0].plan,
                limit_5h: &rows[0].limit_5h,
                limit_week: &rows[0].limit_week,
            },
            widths,
        );
        let cropped = truncate_display(&line, terminal_width);
        assert!(UnicodeWidthStr::width(cropped.as_str()) <= terminal_width);
    }

    #[test]
    fn wrap_display_wraps_help_without_ellipsis() {
        let lines = wrap_display(
            "Keys: move, Enter select, a add, s settings, w webdav, r refresh, t refresh all, Backspace delete",
            24,
        );
        assert!(lines.len() > 1);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 24));
        assert!(!lines.iter().any(|line| line.contains("...")));
    }

    #[test]
    fn wrap_display_splits_long_error_tokens() {
        let lines = wrap_display(
            "Usage refresh failed: https://chatgpt.com/backend-api/wham/usage",
            20,
        );
        assert!(lines.len() > 2);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 20));
    }

    #[test]
    fn proxy_url_from_pairs_prefers_https_proxy() {
        let proxy = proxy_url_from_pairs([
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:8080".to_string(),
            ),
            (
                "ALL_PROXY".to_string(),
                "socks5://127.0.0.1:1080".to_string(),
            ),
            (
                "HTTPS_PROXY".to_string(),
                " http://127.0.0.1:7890 ".to_string(),
            ),
        ]);
        assert_eq!(proxy.as_deref(), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn active_account_key_from_current_auth_matches_registry_email() {
        let dir = tempfile::tempdir().unwrap();
        let current_auth_path = dir.path().join("auth.json");
        let email = "current@example.com";
        let access_token = test_jwt(json!({
            "https://api.openai.com/profile": {
                "email": email
            }
        }));
        fs::write(
            &current_auth_path,
            serde_json::to_vec(&json!({
                "tokens": {
                    "access_token": access_token
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let registry = Registry {
            schema_version: 1,
            active_account_key: Some("other@example.com".to_string()),
            active_account_activated_at_ms: None,
            accounts: vec![AccountRecord {
                account_key: email.to_string(),
                email: email.to_string(),
                alias: String::new(),
                account_name: None,
                plan: None,
                last_used_at: None,
                last_usage: None,
                last_usage_at: None,
                inactive: false,
                extra: BTreeMap::new(),
            }],
            extra: BTreeMap::new(),
        };

        assert_eq!(
            active_account_key_from_current_auth(&registry, &current_auth_path)
                .unwrap()
                .as_deref(),
            Some(email)
        );
    }

    #[test]
    fn usage_limit_text_reads_cached_local_windows() {
        let usage = Some(RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 12.0,
                window_minutes: Some(300),
                resets_at: None,
                extra: BTreeMap::new(),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 34.5,
                window_minutes: Some(10080),
                resets_at: None,
                extra: BTreeMap::new(),
            }),
            plan_type: Some("pro".to_string()),
            extra: BTreeMap::new(),
        });
        assert_eq!(usage_limit_text(&usage, 300, true, 1_700_000_000), "88%");
        assert_eq!(
            usage_limit_text(&usage, 10080, false, 1_700_000_000),
            "65.5%"
        );
        assert_eq!(display_plan(&account_with_usage(usage)), "pro");
    }

    #[test]
    fn codex_usage_response_maps_to_cached_windows() {
        let usage = CodexUsageResponse {
            plan_type: Some("pro".to_string()),
            rate_limit: CodexUsageRateLimit {
                primary_window: CodexUsageWindow {
                    used_percent: 12.0,
                    limit_window_seconds: 300 * 60,
                    reset_at: 1_700_000_000,
                },
                secondary_window: CodexUsageWindow {
                    used_percent: 34.5,
                    limit_window_seconds: 10080 * 60,
                    reset_at: 1_700_100_000,
                },
            },
        };
        let snapshot = rate_limit_snapshot_from_usage(usage);
        let cached_snapshot = Some(snapshot.clone());
        assert_eq!(
            usage_limit_text(&cached_snapshot, 300, true, 1_699_996_700),
            "88% 55m"
        );
        assert_eq!(
            usage_limit_text(&cached_snapshot, 10080, false, 1_700_000_000),
            "65.5% 27h 47m"
        );
        assert_eq!(snapshot.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn remaining_time_formats_hours_and_minutes() {
        assert_eq!(format_remaining_time(-1), "0m");
        assert_eq!(format_remaining_time(60), "1m");
        assert_eq!(format_remaining_time(61), "2m");
        assert_eq!(format_remaining_time(3_660), "1h 1m");
    }

    #[test]
    fn settings_reject_legacy_timezone_field() {
        let settings = r#"{
          "limit_timezone_offset_minutes": 480,
          "webdav": {
            "url": "https://dav.example.com/authswap/",
            "username": "user",
            "password": "pass"
          }
        }"#;
        assert!(serde_json::from_str::<AppSettings>(settings).is_err());
    }

    #[test]
    fn settings_default_does_not_restart_app_server() {
        assert!(!AppSettings::default().restart_app_server_after_switch);
    }

    #[test]
    fn settings_accept_restart_app_server_flag() {
        let settings = r#"{
          "restart_app_server_after_switch": true,
          "webdav": {}
        }"#;
        let parsed = serde_json::from_str::<AppSettings>(settings).unwrap();
        assert!(parsed.restart_app_server_after_switch);
    }

    #[test]
    fn normalize_webdav_url_adds_https_and_trailing_slash() {
        assert_eq!(
            normalize_webdav_url("dav.example.com/authswap"),
            "https://dav.example.com/authswap/"
        );
        assert_eq!(
            normalize_webdav_url("http://dav.example.com/authswap"),
            "http://dav.example.com/authswap/"
        );
    }

    #[test]
    fn import_flat_json_without_email_uses_account_id() {
        let data = r#"{
          "access_token": "header.payload.signature",
          "refresh_token": "rt",
          "id_token": "id",
          "account_id": "acct-123"
        }"#;
        let accounts = import_json_accounts(data, None).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_key, "acct-123");
        assert_eq!(accounts[0].email, "");
        assert_eq!(
            accounts[0].auth["tokens"]["access_token"].as_str(),
            Some("header.payload.signature")
        );
    }

    #[test]
    fn sanitize_json_path_input_allows_relative_paths() {
        assert_eq!(
            sanitize_json_path_input("accounts/example.json").unwrap(),
            PathBuf::from("accounts/example.json")
        );
    }

    #[test]
    fn sanitize_json_path_input_unquotes_absolute_paths() {
        assert_eq!(
            sanitize_json_path_input("\"/tmp/account.json\"").unwrap(),
            PathBuf::from("/tmp/account.json")
        );
    }

    fn account_with_usage(last_usage: Option<RateLimitSnapshot>) -> AccountRecord {
        AccountRecord {
            account_key: "acct".to_string(),
            email: "a@example.com".to_string(),
            alias: String::new(),
            account_name: None,
            plan: Some("plus".to_string()),
            last_used_at: None,
            last_usage,
            last_usage_at: None,
            inactive: false,
            extra: BTreeMap::new(),
        }
    }

    fn test_jwt(payload: Value) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{payload}.signature")
    }
}
