use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use chrono::Local;
use network_interface::NetworkInterfaceConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tokio::time::{interval, MissedTickBehavior};

const LOG_SIZE_LIMIT: u64 = 128 * 1024;
const PING_TARGETS: &[&str] = &["8.8.8.8", "1.1.1.1", "114.114.114.114"];
const CHECK_INTERVAL: u64 = 5;

/// 断网历史最多保留多少条。
/// 原来只 push 从不裁剪，长期运行内存会单调增长（P2-7）。
const MAX_HISTORY: usize = 500;

/// 日志界面最多返回多少行。注意是**最新**的 N 行（P1-8），不是最旧的。
const MAX_LOG_LINES: usize = 500;

/// Web 服务默认只监听回环地址（P1-1）。
const LOOPBACK_ADDR: [u8; 4] = [127, 0, 0, 1];
/// 只有在用户显式开启"允许局域网访问"时才监听全部网卡。
const ANY_ADDR: [u8; 4] = [0, 0, 0, 0];

/// 前后端**共用**的语言资源（P2-6）。
///
/// 这份 JSON 同时被两个地方读取：
/// - Rust 端用 `include_str!` 编进二进制，供托盘菜单和日志翻译使用；
/// - 前端直接 `import`，供界面文案使用。
///
/// 从此不存在"两边各维护一份、靠人工同步、还已经不一致了"的问题。
const I18N_JSON: &str = include_str!("../../i18n.json");

/// 日志文件的写入锁。
///
/// 监控任务、Tauri 命令、HTTP 接口都可能同时写日志；`OpenOptions::append`
/// 的 `write_all` 并不保证跨线程的整行原子性，并发时会互相撕裂。
static LOG_LOCK: Mutex<()> = Mutex::new(());

// ───────────────────────────── 数据模型 ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: String,
    pub port: u16,
    pub autostart: bool,
    /// 是否允许局域网访问内嵌 Web 服务，默认关闭（P1-1）。
    pub web_access: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: "zh".to_string(),
            port: 8080,
            autostart: false,
            web_access: false,
        }
    }
}

impl Settings {
    /// 把不可用的值掰回合法值。用于**读取配置**时兜底，
    /// 避免用户手改配置文件写坏了之后程序起不来。
    fn normalize(mut self) -> Self {
        self.language = if self.language.eq_ignore_ascii_case("en") {
            "en".to_string()
        } else {
            "zh".to_string()
        };
        if self.port == 0 {
            self.port = 8080;
        }
        self
    }

    /// 保存前校验。这里不静默修正，而是报错让用户知道输入有问题。
    fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port must be between 1 and 65535".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub is_online: bool,
    pub adapter_status: bool,
    pub ping_success: bool,
    pub last_check: String,
    pub disconnect_count: u32,
    /// 当前这次断网已经持续了多少秒；在线时为 `None`（P1-9）。
    pub current_disconnect_duration: Option<u64>,
    pub disconnect_history: Vec<DisconnectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisconnectRecord {
    pub start: String,
    pub end: Option<String>,
    pub duration_secs: Option<u64>,
}

/// 日志行结构化之后的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    /// i18n 的 key，例如 `log_disconnected`。
    pub message: String,
    /// 按当前语言翻译后的文案。
    pub translated: String,
}

/// 落盘的断网统计，重启后能接着算（P2-7）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PersistedHistory {
    disconnect_count: u32,
    disconnect_history: Vec<DisconnectRecord>,
}

pub struct AppState {
    pub settings: Mutex<Settings>,
    pub status: Mutex<NetworkStatus>,
    pub i18n: I18n,
    /// 关闭当前 Web 监听的句柄。改端口或改访问范围时先停旧的再起新的。
    pub server_shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// 局域网访问令牌（P1-1）。首次运行随机生成并落盘。
    pub token: String,
}

#[derive(Clone)]
pub struct I18n {
    dict: HashMap<String, HashMap<String, String>>,
}

impl I18n {
    pub fn load() -> Self {
        let dict = serde_json::from_str::<HashMap<String, HashMap<String, String>>>(I18N_JSON)
            .unwrap_or_default();
        Self { dict }
    }

    /// 按 key 查表。查不到时返回 key 本身，方便一眼看出漏了哪条翻译。
    pub fn t(&self, lang: &str, key: &str) -> String {
        self.dict
            .get(lang)
            .and_then(|d| d.get(key))
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
}

// ───────────────────────────── 路径 / 配置 ─────────────────────────────

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("NetSentry")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

fn history_path() -> PathBuf {
    config_dir().join("history.json")
}

fn token_path() -> PathBuf {
    config_dir().join("token.txt")
}

fn get_log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("NetSentry")
        .join("logs")
}

fn ensure_log_dir() -> std::io::Result<PathBuf> {
    let dir = get_log_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn get_current_log_file() -> std::io::Result<PathBuf> {
    let dir = ensure_log_dir()?;
    Ok(dir.join("netsentry.log"))
}

fn rotate_log_if_needed(log_path: &PathBuf) -> std::io::Result<PathBuf> {
    if log_path.exists() {
        let metadata = fs::metadata(log_path)?;
        if metadata.len() >= LOG_SIZE_LIMIT {
            let timestamp = Local::now().format("%Y%m%d_%H%M%S");
            let archive_path = get_log_dir().join(format!("netsentry_{}.log", timestamp));
            fs::rename(log_path, &archive_path)?;
        }
    }
    Ok(log_path.clone())
}

// ───────────────────────────── 日志 ─────────────────────────────

/// 写一条日志。**唯一**的日志写入口。
///
/// 日志行格式固定为：`[时间戳] [级别] key[ | detail]`
///
/// 两个关键设计：
/// 1. **只存 i18n 的 key，不存文案**。日志要能跟着界面语言切换（P1-7），
///    只有把 key 落盘，读取时才能按当前语言重新翻译。原来存的是完整英文句子，
///    再拿它去查字典，永远查不到，翻译功能实际从未生效。
/// 2. **不再调用 `log::` 宏**。原来的 `CombinedLogger` 指向同一个文件，
///    导致每条日志被以两种格式各写一遍、并发时还会互相撕裂（P1-5）。
fn write_log(level: &str, key: &str, detail: Option<&str>) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = match detail {
        Some(d) if !d.is_empty() => format!("[{}] [{}] {} | {}\n", timestamp, level, key, d),
        _ => format!("[{}] [{}] {}\n", timestamp, level, key),
    };

    lock_log(|| {
        let log_path = get_current_log_file().unwrap_or_else(|_| PathBuf::from("netsentry.log"));
        let path = rotate_log_if_needed(&log_path).unwrap_or(log_path);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
        }
    });
}

/// 拿日志锁执行一段写操作。锁中毒时直接取回内部数据，不让日志把程序带崩。
fn lock_log<F: FnOnce()>(f: F) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f();
}

/// 解析一行日志。
///
/// 时间戳里**含一个空格**（`2026-09-11 13:04:05.123`），所以不能按空格切分，
/// 否则会把日期和时间切开、前端只能显示半截（P1-6）。这里按 `"] "` 切分，
/// 天然把"方括号之间的内容"当成一段。
fn parse_log_line(line: &str, i18n: &I18n, lang: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, "] ");
    let timestamp = parts.next()?.trim_start_matches('[').to_string();
    let level = parts.next()?.trim_start_matches('[').to_string();
    let rest = parts.next()?;

    let (key, detail) = match rest.split_once(" | ") {
        Some((k, d)) => (k, Some(d)),
        None => (rest, None),
    };

    let translated = match detail {
        Some(d) => format!("{} {}", i18n.t(lang, key), d),
        None => i18n.t(lang, key),
    };

    Some(LogEntry {
        timestamp,
        level,
        message: key.to_string(),
        translated,
    })
}

/// 读取日志并按当前语言翻译。返回结果为**最新在前**。
fn read_translated_logs(state: &AppState) -> Vec<LogEntry> {
    let lang = state.settings.lock().unwrap().language.clone();
    let log_path = get_current_log_file().unwrap_or_else(|_| PathBuf::from("netsentry.log"));

    let mut lines: Vec<String> = match File::open(&log_path) {
        Ok(file) => BufReader::new(file).lines().map_while(Result::ok).collect(),
        Err(_) => return Vec::new(),
    };

    // 只保留最后 N 行：原来是从头 take(N) 再 reverse，拿到的是**最旧**的 N 条，
    // 日志一超过 N 行就再也看不到新记录了（P1-8）。
    if lines.len() > MAX_LOG_LINES {
        lines.drain(0..lines.len() - MAX_LOG_LINES);
    }

    let mut entries: Vec<LogEntry> = lines
        .iter()
        .filter_map(|line| parse_log_line(line, &state.i18n, &lang))
        .collect();
    entries.reverse();
    entries
}

// ───────────────────────────── 网络检测 ─────────────────────────────

/// 判断一个 IPv4 地址是否代表"真实可用的本地连接"。
///
/// 需要排除的几种地址：
/// - `0.0.0.0`          未指定地址
/// - `127.0.0.0/8`      回环
/// - `169.254.0.0/16`   APIPA / 链路本地。Windows 在拿不到 DHCP 地址时会自动分配这个段，
///                      拔网线、WiFi 没连上、虚拟网卡都是这个段，必须排除，否则永远判为"在线"。
/// - `255.255.255.255`  广播
fn is_usable_ipv4(ip: &Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_link_local() && !ip.is_broadcast()
}

/// 是否存在任意一张网卡拿到了可用的 IPv4 地址。
///
/// 两个必须避开的坑：
/// 1. **不能按网卡名称匹配**。中文 Windows 上有线网卡叫"以太网"、本地连接叫"本地连接"，
///    匹配 "ethernet"/"local" 之类的英文关键字永远匹配不上，会恒判为离线。
/// 2. **不能用 `addr.is_empty()` 判断**。IPv6 链路本地地址（fe80::/64）几乎总存在，
///    这个判据恒为真，等于"网卡存在即在线"，检测不到任何真实故障。
fn check_adapter_status() -> bool {
    match network_interface::NetworkInterface::show() {
        Ok(interfaces) => interfaces.iter().any(|iface| {
            iface
                .addr
                .iter()
                .any(|a| matches!(a, network_interface::Addr::V4(v4) if is_usable_ipv4(&v4.ip)))
        }),
        Err(_) => false,
    }
}

/// 对单个目标做一次 ping。
#[cfg(target_os = "windows")]
async fn ping_once(target: &str) -> bool {
    let mut cmd = tokio::process::Command::new("ping");
    // -n 次数 / -w 单次超时(毫秒)
    cmd.args(["-n", "1", "-w", "1000", target]);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW，避免弹黑框
    matches!(cmd.output().await, Ok(o) if o.status.success())
}

/// 对单个目标做一次 ping。
#[cfg(not(target_os = "windows"))]
async fn ping_once(target: &str) -> bool {
    let mut cmd = tokio::process::Command::new("ping");
    // Linux / macOS 的语义和 Windows 完全不同（P1-10）：
    // -c 是次数、-W 是单次超时（秒）。原来的 `-n 1 -w 1000` 在 Linux 上
    // 意味着"超时 1000 秒"，一次调用能挂十几分钟。
    cmd.args(["-c", "1", "-W", "1", target]);
    matches!(cmd.output().await, Ok(o) if o.status.success())
}

/// 所有候选目标里任意一个 ping 通就算通。
///
/// 用 `tokio::process` 而不是 `std::process`（P2-8）：原来的阻塞版 `output()`
/// 跑在 tokio 的工作线程上，最坏会把该线程卡住约 3 秒。
async fn check_ping() -> bool {
    for target in PING_TARGETS {
        if ping_once(target).await {
            return true;
        }
    }
    false
}

/// 返回 `(整体是否在线, 网卡是否正常, ping 是否成功)`。
///
/// 原来只返回两个值，把真实的 ping 结果丢掉了，前端拿到的 `ping_success`
/// 其实是"整体是否在线"，UI 上 "Ping" 那一行显示的语义是错的（P1-4）。
async fn check_network_status() -> (bool, bool, bool) {
    let adapter_ok = check_adapter_status();
    let ping_ok = check_ping().await;
    (adapter_ok && ping_ok, adapter_ok, ping_ok)
}

// ───────────────────────────── 开机自启 ─────────────────────────────

#[cfg(target_os = "windows")]
fn set_autostart(enable: bool) -> Result<(), String> {
    use winreg::enums::*;
    use winreg::RegKey;

    let exe_path = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .to_string();

    let key_path = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    if enable {
        // create_subkey 而不是 open_subkey_with_flags：后者在 key 不存在时会直接失败（P2-13）。
        let (reg_key, _) = hkcu.create_subkey(key_path).map_err(|e| e.to_string())?;
        // 路径加引号，否则 exe 路径含空格时会被当成多个参数（P2-13）。
        reg_key
            .set_value("NetSentry", &format!("\"{}\"", exe_path))
            .map_err(|e| e.to_string())?;
    } else if let Ok(reg_key) = hkcu.open_subkey_with_flags(key_path, KEY_SET_VALUE) {
        // 关闭时 key 可能压根不存在，这里不算错误。
        let _ = reg_key.delete_value("NetSentry");
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn set_autostart(_enable: bool) -> Result<(), String> {
    Ok(())
}

// ───────────────────────────── 令牌 ─────────────────────────────

/// 生成一个 32 位十六进制随机令牌。
///
/// 不额外引入 `rand` 依赖，用系统时间纳秒 + 进程号 + 栈地址（ASLR）拼熵，
/// 对"防止局域网内随手访问"这个量级的防护足够了。
fn generate_token() -> String {
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let stack_probe: u8 = 0;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    (&stack_probe as *const u8 as usize).hash(&mut hasher);
    let first = hasher.finish();

    let mut second = std::collections::hash_map::DefaultHasher::new();
    first.hash(&mut second);
    (first ^ 0x9E37_79B9_7F4A_7C15).hash(&mut second);
    first.hash(&mut second);

    format!("{:016x}{:016x}", first, second.finish())
}

/// 读取已有令牌；没有就生成一个并落盘，方便用户去配置文件里查。
fn load_or_create_token() -> String {
    let path = token_path();
    if let Ok(raw) = fs::read_to_string(&path) {
        let existing = raw.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }

    let token = generate_token();
    let _ = fs::create_dir_all(config_dir());
    let _ = fs::write(&path, &token);
    token
}

// ───────────────────────────── 设置读写 ─────────────────────────────

fn load_settings() -> Settings {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_settings_to_file(settings: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(config_dir())?;
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(settings_path(), json)
}

fn load_history() -> PersistedHistory {
    fs::read_to_string(history_path())
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

/// 把断网统计落盘（P2-7）。只在状态跃迁时调用，频率很低。
fn persist_history(state: &AppState) {
    let snapshot = {
        let status = state.status.lock().unwrap();
        PersistedHistory {
            disconnect_count: status.disconnect_count,
            disconnect_history: status.disconnect_history.clone(),
        }
    };

    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let _ = fs::create_dir_all(config_dir());
        let _ = fs::write(history_path(), json);
    }
}

/// Tauri 命令和 HTTP 接口**共用**的保存逻辑。
///
/// 顺序很重要（P2-12）：先动容易失败的外部状态（注册表），再落盘，
/// 最后才更新内存。任何一步失败都会在内存被改脏之前返回。
fn apply_settings(
    state: &Arc<AppState>,
    app: &AppHandle,
    new_settings: Settings,
) -> Result<String, String> {
    new_settings.validate()?;
    let new_settings = new_settings.normalize();

    let old_settings = state.settings.lock().unwrap().clone();

    // 1) 注册表。失败就直接返回，此时磁盘和内存都还没动。
    if old_settings.autostart != new_settings.autostart {
        set_autostart(new_settings.autostart)?;
        write_log(
            "INFO",
            if new_settings.autostart {
                "log_autostart_enabled"
            } else {
                "log_autostart_disabled"
            },
            None,
        );
    }

    // 2) 落盘。原来这一步完全没有，导致设置改完重启就丢（P0-1）。
    save_settings_to_file(&new_settings).map_err(|e| e.to_string())?;

    // 3) 更新内存
    *state.settings.lock().unwrap() = new_settings.clone();

    if old_settings.language != new_settings.language {
        write_log("INFO", "log_language_changed", Some(&new_settings.language));
    }
    write_log("INFO", "log_settings_saved", None);

    // 4) 端口或访问范围变了就重启 Web 服务。原来只在启动时 bind 一次，
    //    改端口完全不生效（P0-3）。
    if old_settings.port != new_settings.port || old_settings.web_access != new_settings.web_access {
        start_web_server(app.clone(), state.clone(), new_settings.port);
    }

    let _ = app.emit("settings-changed", new_settings.clone());

    Ok(state.i18n.t(&new_settings.language, "settings_saved"))
}

// ───────────────────────────── Tauri 命令 ─────────────────────────────

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(
    settings: Settings,
    state: tauri::State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<String, String> {
    apply_settings(state.inner(), &app, settings)
}

#[tauri::command]
fn get_status(state: tauri::State<'_, Arc<AppState>>) -> NetworkStatus {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn get_translated_logs(state: tauri::State<'_, Arc<AppState>>) -> Vec<LogEntry> {
    read_translated_logs(state.inner())
}

// ───────────────────────────── 内嵌 Web 服务 ─────────────────────────────

/// 把请求路径映射成资源路径。
///
/// 返回 `None` 表示这个路径**不允许**访问（P1-2 路径穿越防护）：
/// 任何 `..`、`.`、反斜杠、盘符都被拒绝，保证绝不会读到前端资源目录之外的文件。
/// 原来直接 `dist_path.join(path.trim_start_matches('/'))`，
/// 请求 `/../../Windows/win.ini` 就能读到任意文件。
fn sanitize_asset_path(raw_path: &str) -> Option<String> {
    // uri().path() 一般不带 query，但这里再兜一层，防止调用方传入完整 URI。
    let trimmed = raw_path
        .trim_start_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("");

    if trimmed.is_empty() {
        return Some("index.html".to_string());
    }

    let mut segments: Vec<&str> = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." || seg.contains('\\') || seg.contains(':') || seg.contains('\0')
        {
            return None;
        }
        segments.push(seg);
    }

    if segments.is_empty() {
        return Some("index.html".to_string());
    }

    Some(segments.join("/"))
}

/// 返回前端静态资源。
///
/// 用 Tauri 的 `asset_resolver` 而不是 `current_dir()/dist`（P1-12）：
/// 开发态它从 `frontendDist` 目录读盘，打包后它读编译进二进制的资源，
/// 两种形态都能正确工作。原来的写法在安装版上只会返回一个提示页。
fn serve_static(app: &AppHandle, raw_path: &str) -> Response {
    let Some(asset_path) = sanitize_asset_path(raw_path) else {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    };

    match app.asset_resolver().get(asset_path) {
        Some(asset) => {
            let mut builder = Response::builder().header(header::CONTENT_TYPE, asset.mime_type);
            if let Some(csp) = asset.csp_header {
                if let Ok(value) = HeaderValue::from_str(&csp) {
                    builder = builder.header(header::CONTENT_SECURITY_POLICY, value);
                }
            }
            builder
                .body(Body::from(asset.bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// `/api/*` 的访问控制（P1-1）。
///
/// - 默认只监听 `127.0.0.1`，此时所有请求都来自回环地址，直接放行，
///   本机浏览器打开 `http://127.0.0.1:<port>` 不需要任何额外配置；
/// - 一旦开启"允许局域网访问"，来自非回环地址的请求必须携带令牌：
///   `Authorization: Bearer <token>`，或 `?token=<token>`。
///
/// 静态资源本身不校验，否则用户连页面都打不开。
async fn api_auth(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if addr.ip().is_loopback() {
        return next.run(req).await;
    }

    let expected = state.token.clone();
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| {
            req.uri().query().and_then(|q| {
                q.split('&').find_map(|pair| {
                    let mut it = pair.splitn(2, '=');
                    match (it.next(), it.next()) {
                        (Some("token"), Some(v)) => Some(v.to_string()),
                        _ => None,
                    }
                })
            })
        });

    if provided.as_deref() == Some(expected.as_str()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: provide Authorization: Bearer <token>",
        )
            .into_response()
    }
}

/// 启动（或重启）内嵌 Web 服务。
///
/// 会先给旧的监听发关闭信号，再在新端口上 bind。
/// bind 失败不再 unwrap（原来在独立线程里 panic，界面完全无感知），
/// 而是记日志 + 发 `web-server-error` 事件通知前端。
fn start_web_server(app: AppHandle, state: Arc<AppState>, port: u16) {
    if let Some(tx) = state.server_shutdown.lock().unwrap().take() {
        let _ = tx.send(());
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    *state.server_shutdown.lock().unwrap() = Some(tx);

    let web_access = state.settings.lock().unwrap().web_access;

    // 复用 Tauri 自带的 tokio runtime（P2-10），不再为每个任务各建一个 Runtime。
    tauri::async_runtime::spawn(async move {
        let s_status = state.clone();
        let s_settings = state.clone();
        let s_save = state.clone();
        let s_logs = state.clone();

        let app_for_save = app.clone();
        let app_for_static = app.clone();
        let app_for_emit = app.clone();

        let api = axum::Router::new()
            .route(
                "/api/status",
                axum::routing::get(move || {
                    let s = s_status.clone();
                    async move { axum::Json(serde_json::json!({ "status": s.status.lock().unwrap().clone() })) }
                }),
            )
            .route(
                "/api/settings",
                axum::routing::get(move || {
                    let s = s_settings.clone();
                    async move { axum::Json(s.settings.lock().unwrap().clone()) }
                }),
            )
            .route(
                "/api/save-settings",
                axum::routing::post(move |axum::Json(settings): axum::Json<Settings>| {
                    let s = s_save.clone();
                    let app = app_for_save.clone();
                    async move {
                        match apply_settings(&s, &app, settings) {
                            Ok(_) => (
                                StatusCode::OK,
                                axum::Json(serde_json::json!({ "success": true })),
                            )
                                .into_response(),
                            Err(e) => (
                                StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({ "success": false, "error": e })),
                            )
                                .into_response(),
                        }
                    }
                }),
            )
            // 返回结构化日志，和 Tauri 命令 get_translated_logs 的形状保持一致，
            // 这样浏览器访问和窗口访问拿到的是同一份数据。
            .route(
                "/api/logs",
                axum::routing::get(move || {
                    let s = s_logs.clone();
                    async move { axum::Json(read_translated_logs(&s)) }
                }),
            )
            // route_layer 只作用于上面已注册的 /api/* 路由，不动 fallback。
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                api_auth,
            ));

        let router = api.fallback(move |req: Request| {
            let app = app_for_static.clone();
            async move { serve_static(&app, req.uri().path()) }
        });

        let addr = SocketAddr::from((if web_access { ANY_ADDR } else { LOOPBACK_ADDR }, port));

        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                write_log("INFO", "log_web_started", Some(&addr.to_string()));
                let _ = app_for_emit.emit("web-server-started", addr.to_string());

                match axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                {
                    Ok(()) => write_log("INFO", "log_web_stopped", Some(&addr.to_string())),
                    Err(e) => write_log("ERROR", "log_web_stopped", Some(&e.to_string())),
                }
            }
            Err(e) => {
                let detail = format!("{}: {}", port, e);
                write_log("ERROR", "log_bind_failed", Some(&detail));
                let _ = app_for_emit.emit("web-server-error", detail);
            }
        }
    });
}

// ───────────────────────────── 网络监控循环 ─────────────────────────────

fn start_network_monitoring(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = interval(Duration::from_secs(CHECK_INTERVAL));
        // 检测本身有可能超过 5 秒；默认的 Burst 会一次性补发多个 tick，
        // 造成日志刷屏。改成 Delay：错过就错过，下一轮从当前时刻重新计（P2-9）。
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut disconnect_start_time: Option<Instant> = None;

        loop {
            ticker.tick().await;

            // 先把需要 await 的网络检测跑完，再去拿锁，
            // 避免把 `std::sync::Mutex` 的 guard 跨 await 持有。
            let (is_online, adapter_ok, ping_ok) = check_network_status().await;
            let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

            let went_offline;
            let came_online;
            let mut recovered_after: Option<u64> = None;

            {
                let mut status = state.status.lock().unwrap();
                let old_online = status.is_online;

                status.is_online = is_online;
                status.adapter_status = adapter_ok;
                status.ping_success = ping_ok;
                status.last_check = now.clone();

                went_offline = !is_online && old_online;
                came_online = is_online && !old_online;

                if went_offline {
                    if disconnect_start_time.is_none() {
                        disconnect_start_time = Some(Instant::now());
                    }

                    status.disconnect_history.push(DisconnectRecord {
                        start: now.clone(),
                        end: None,
                        duration_secs: None,
                    });

                    let overflow = status.disconnect_history.len().saturating_sub(MAX_HISTORY);
                    if overflow > 0 {
                        status.disconnect_history.drain(0..overflow);
                    }

                    status.disconnect_count = status.disconnect_count.saturating_add(1);
                    // 刚断开的那一刻就是 0 秒，而不是 None（P2-18 的前端侧对应问题）
                    status.current_disconnect_duration = Some(0);
                } else if came_online {
                    if let Some(start) = disconnect_start_time.take() {
                        let duration = start.elapsed().as_secs();
                        if let Some(last) = status.disconnect_history.last_mut() {
                            last.end = Some(now.clone());
                            last.duration_secs = Some(duration);
                        }
                        recovered_after = Some(duration);
                    } else if let Some(last) = status.disconnect_history.last_mut() {
                        // 极端情况：本进程启动时就已经处于断网状态，没有 start 记录
                        last.end = Some(now.clone());
                    }

                    // 恢复后必须清零（P1-9），否则统计卡片永远停在上一次的秒数
                    status.current_disconnect_duration = None;
                } else if !is_online {
                    if let Some(start) = &disconnect_start_time {
                        status.current_disconnect_duration = Some(start.elapsed().as_secs());
                    }
                }
            }
            // ── 锁已释放，下面再做日志 / 落盘这类 IO（P2-11）──

            if went_offline {
                write_log("WARN", "log_disconnected", None);
                persist_history(&state);
            } else if came_online {
                let detail = recovered_after.map(|d| format!("{}s", d));
                write_log("INFO", "log_restored", detail.as_deref());
                persist_history(&state);
            }

            // 每轮都推一次完整状态，前端因此不需要自己 setInterval 轮询（P2-19）。
            let snapshot = state.status.lock().unwrap().clone();
            let _ = app.emit("network-status-update", snapshot);
        }
    });
}

// ───────────────────────────── 托盘 ─────────────────────────────

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 托盘里切换语言。走和设置界面同一条保存链路，这样关机重启后语言还在。
fn set_language_from_tray(app: &AppHandle, lang: &str) {
    let state = app.state::<Arc<AppState>>();
    let mut settings = state.settings.lock().unwrap().clone();
    if settings.language == lang {
        return;
    }
    settings.language = lang.to_string();

    if let Err(e) = apply_settings(state.inner(), app, settings) {
        write_log("ERROR", "log_settings_saved", Some(&e));
    }
}

fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let state = app.state::<Arc<AppState>>();
    let lang = state.settings.lock().unwrap().language.clone();
    let i18n = &state.i18n;

    let settings_item = MenuItem::with_id(app, "settings", i18n.t(&lang, "settings"), true, None::<&str>)?;
    let logs_item = MenuItem::with_id(app, "logs", i18n.t(&lang, "view_logs"), true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let lang_zh_item = MenuItem::with_id(app, "lang_zh", "中文", true, None::<&str>)?;
    let lang_en_item = MenuItem::with_id(app, "lang_en", "English", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let about_item = MenuItem::with_id(app, "about", i18n.t(&lang, "about"), true, None::<&str>)?;
    // 退出项也走 i18n（P2-14），不再硬编码 "Quit"
    let quit_item = MenuItem::with_id(app, "quit", i18n.t(&lang, "quit"), true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &settings_item,
            &logs_item,
            &separator1,
            &lang_zh_item,
            &lang_en_item,
            &separator2,
            &about_item,
            &quit_item,
        ],
    )?;

    let _tray = {
        let mut builder = TrayIconBuilder::new()
            .menu(&menu)
            .tooltip("NetSentry")
            .show_menu_on_left_click(false);

        // 没有图标时不要 panic：托盘图标缺失只影响观感，不该让整个启动流程失败。
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }

        builder
            .on_menu_event(|app, event| match event.id.as_ref() {
                // 原来还有一个 "show" 分支，但菜单里没有任何 id 为 show 的项，
                // 是永远走不到的死代码（P2-15），已删除。
                "settings" => {
                    show_main_window(app);
                    let _ = app.emit("open-settings", ());
                }
                "logs" => {
                    show_main_window(app);
                    let _ = app.emit("open-logs", ());
                }
                "lang_zh" => set_language_from_tray(app, "zh"),
                "lang_en" => set_language_from_tray(app, "en"),
                "about" => {
                    show_main_window(app);
                    let _ = app.emit("open-about", ());
                }
                "quit" => {
                    write_log("INFO", "log_app_exiting", None);
                    app.exit(0);
                }
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show_main_window(tray.app_handle());
                }
            })
            .build(app)?
    };

    Ok(())
}

// ───────────────────────────── 入口 ─────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = ensure_log_dir();

    let settings = load_settings().normalize();
    let port = settings.port;
    let _ = save_settings_to_file(&settings);

    let persisted = load_history();

    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        status: Mutex::new(NetworkStatus {
            is_online: true,
            adapter_status: true,
            ping_success: true,
            last_check: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            disconnect_count: persisted.disconnect_count,
            current_disconnect_duration: None,
            disconnect_history: persisted.disconnect_history,
        }),
        i18n: I18n::load(),
        server_shutdown: Mutex::new(None),
        token: load_or_create_token(),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(move |app| {
            let state_arc = app.state::<Arc<AppState>>().inner().clone();

            start_web_server(app.handle().clone(), state_arc.clone(), port);

            if let Err(e) = setup_tray(app.handle()) {
                write_log("ERROR", "log_tray_failed", Some(&e.to_string()));
            }

            start_network_monitoring(app.handle().clone(), state_arc);

            write_log("INFO", "log_app_started", None);

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭主窗口时只隐藏、不销毁（P1-11）。
            // 窗口一旦被销毁，之后所有 `get_webview_window("main")` 都返回 None，
            // 托盘的"设置 / 日志 / 关于"和左键唤起全部失效，只能杀进程重启。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_status,
            get_translated_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
