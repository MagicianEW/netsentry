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
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpStream;
use tokio::time::{interval, sleep, MissedTickBehavior};

const LOG_SIZE_LIMIT: u64 = 128 * 1024;
const PING_TARGETS: &[&str] = &["8.8.8.8", "1.1.1.1", "114.114.114.114"];

/// TCP 兜底探测目标（`IP:端口`，P1-4）。
///
/// 只 ping 的问题是致命的：企业防火墙屏蔽 ICMP echo 是常态，目标全部不可达时
/// 工具会把「网络完全正常」报成持续断网，并持续污染断网次数与历史统计。
/// 只要能建连，就说明链路和路由是通的。
const TCP_TARGETS: &[(&str, u16)] = &[
    ("223.5.5.5", 53),  // AliDNS
    ("119.29.29.29", 53), // DNSPod
    ("1.1.1.1", 443),
];
const CHECK_INTERVAL: u64 = 5;

/// 断网历史最多保留多少条。
/// 原来只 push 从不裁剪，长期运行内存会单调增长（P2-7）。
const MAX_HISTORY: usize = 500;

/// 日志界面最多返回多少行。注意是**最新**的 N 行（P1-8），不是最旧的。
const MAX_LOG_LINES: usize = 500;

/// 最多保留多少个历史日志归档。
///
/// 轮转只把当前文件改名成 `netsentry_<时间戳>.log`，原来既没有读取路径、
/// 也不清理，目录会无限增长，而界面永远只能看到轮转后的那一个文件。
const MAX_LOG_ARCHIVES: usize = 10;

/// bind 失败后的重试次数与间隔。
///
/// 启动阶段 bind 失败原本只发一个事件，而事件早于前端监听注册会直接丢失，
/// 窗口又是隐藏的，用户什么都看不到、坏端口还会被落盘（P1-5）。
const BIND_RETRIES: usize = 5;
const BIND_RETRY_DELAY_MS: u64 = 400;

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
    /// TCP 兜底探测结果（P1-4）。ICMP 被企业防火墙屏蔽是常态，
    /// 只看 ping 会把「网络完全正常」误报成持续断网并污染统计。
    /// 单独暴露出来是为了让界面如实显示"ping 失败但网络是通的"，而不是掩盖。
    pub tcp_success: bool,
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
    /// 断网起始的 epoch 秒。
    ///
    /// 跨进程重启时 `start` 只是一个给人看的格式化字符串，没法参与运算；
    /// 有了它，「断网中途进程被杀」的那一段时长才能被补回来（P0-1）。
    #[serde(default)]
    pub start_epoch: Option<i64>,
    /// 恢复时刻的 epoch 秒，作用同上。
    #[serde(default)]
    pub end_epoch: Option<i64>,
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
    /// 当前 Web 服务任务的句柄。改绑之前必须**等它真正退出**，
    /// 否则同端口换绑（127.0.0.1:P → 0.0.0.0:P）会撞上仍在监听的旧 socket（P1-6）。
    pub server_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Web 服务的实际运行状态。启动阶段 emit 的事件早于前端监听注册会丢失，
    /// 所以界面改为**主动查询**这个状态（P1-5）。
    pub web_status: Mutex<WebStatus>,
    /// 局域网访问令牌（P1-1）。首次运行随机生成并落盘。
    pub token: String,
    /// 当前这次断网的起始 epoch 秒。
    ///
    /// 原来这个时间点只以局部变量 `Instant` 的形式活在监控任务里，进程一退出就丢；
    /// 叠加 `run()` 里 `is_online` 硬编码为 true，导致「断网中途重启」的记录
    /// 永远闭合不了、界面恒显"仍在断网中"（P0-1）。放进 AppState 后
    /// 它随 `history.json` 一起跨重启续接。
    pub outage_start_epoch: Mutex<Option<i64>>,
    /// 托盘图标句柄。托盘菜单需要能按当前语言重建（P1-8）。
    pub tray: Mutex<Option<TrayIcon>>,
}

/// 内嵌 Web 服务的对外可见状态。
#[derive(Debug, Clone, Default)]
pub enum WebStatus {
    /// 还没启动 / 已按设置停掉。
    #[default]
    Stopped,
    /// 正在启动（含改绑时等待旧服务退出的阶段）。
    Starting,
    /// 已监听，携带 `127.0.0.1:8080` 这样的地址。
    Running(String),
    /// 启动失败，携带错误原因，必须让用户看见。
    Failed(String),
}

/// 交给界面的 Web 服务状态快照。
#[derive(Debug, Clone, Serialize)]
pub struct WebStatusInfo {
    /// 是否真的在监听。界面用它决定要不要显示告警条。
    pub running: bool,
    /// `stopped` / `starting` / `running` / `failed`。
    pub phase: &'static str,
    /// 监听地址或失败原因。
    pub detail: Option<String>,
    /// 按当前语言拼好的一行摘要，界面直接显示，不需要再查字典。
    pub summary: String,
}

impl WebStatus {
    /// 界面上能直接展示的一行摘要。
    fn summary(&self, i18n: &I18n, lang: &str) -> String {
        match self {
            WebStatus::Stopped => i18n.t(lang, "web_stopped"),
            WebStatus::Starting => i18n.t(lang, "web_starting"),
            WebStatus::Running(addr) => format!("{} {}", i18n.t(lang, "web_running"), addr),
            WebStatus::Failed(detail) => format!("{} {}", i18n.t(lang, "web_failed"), detail),
        }
    }

    fn info(&self, i18n: &I18n, lang: &str) -> WebStatusInfo {
        let (running, phase, detail) = match self {
            WebStatus::Stopped => (false, "stopped", None),
            WebStatus::Starting => (false, "starting", None),
            WebStatus::Running(addr) => (true, "running", Some(addr.clone())),
            WebStatus::Failed(detail) => (false, "failed", Some(detail.clone())),
        };
        WebStatusInfo {
            running,
            phase,
            detail,
            summary: self.summary(i18n, lang),
        }
    }
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

/// 列出历史日志归档，按时间**从旧到新**。
///
/// 归档名固定是 `netsentry_<YYYYMMDD_HHMMSS>.log`，字典序恰好就是时间序。
fn log_archives() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(get_log_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("netsentry_") && n.ends_with(".log"))
        })
        .collect();
    files.sort();
    files
}

/// 归档数量超过上限时删掉最旧的。轮转原本只改名、从不清理，目录会无限增长。
fn prune_log_archives() {
    let files = log_archives();
    let overflow = files.len().saturating_sub(MAX_LOG_ARCHIVES);
    for path in files.into_iter().take(overflow) {
        let _ = fs::remove_file(path);
    }
}

fn rotate_log_if_needed(log_path: &PathBuf) -> std::io::Result<PathBuf> {
    if log_path.exists() {
        let metadata = fs::metadata(log_path)?;
        if metadata.len() >= LOG_SIZE_LIMIT {
            let stamp = Local::now().format("%Y%m%d_%H%M%S");
            let dir = get_log_dir();
            // 同一秒内轮转两次时加后缀：Unix 上 rename 会静默覆盖掉上一个归档，
            // Windows 上则直接失败。
            let mut archive_path = dir.join(format!("netsentry_{stamp}.log"));
            let mut n = 1;
            while archive_path.exists() {
                archive_path = dir.join(format!("netsentry_{stamp}_{n}.log"));
                n += 1;
            }
            fs::rename(log_path, &archive_path)?;
            prune_log_archives();
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

/// 读一个日志文件的全部行；文件不存在或读不了就当作空。
fn read_log_lines(path: &PathBuf) -> Vec<String> {
    match File::open(path) {
        Ok(file) => BufReader::new(file).lines().map_while(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// 读取日志并按当前语言翻译。返回结果为**最新在前**。
fn read_translated_logs(state: &AppState) -> Vec<LogEntry> {
    let lang = state.settings.lock().unwrap().language.clone();

    // 轮转出来的归档以前**没有任何读取路径**，界面永远只能看到轮转后的那一个文件，
    // 更早的日志等于凭空消失。归档按时间从旧到新拼接，再统一截取最新的 N 行。
    let mut lines: Vec<String> = Vec::new();
    for archive in log_archives() {
        lines.extend(read_log_lines(&archive));
    }
    if let Ok(current) = get_current_log_file() {
        lines.extend(read_log_lines(&current));
    }

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
///   拔网线、WiFi 没连上、虚拟网卡都是这个段，必须排除，否则永远判为"在线"。
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
async fn check_ping() -> bool {
    for target in PING_TARGETS {
        if ping_once(target).await {
            return true;
        }
    }
    false
}

/// TCP 兜底探测（P1-4）。
///
/// 只看 ping 的问题是致命的：企业防火墙屏蔽 ICMP echo 是常态，三个目标全部
/// 不可达时，工具会把「网络完全正常」报成持续断网，并持续污染断网次数与历史统计。
/// 能不能建连和能不能 ping 是两件事，后者被封不代表前者不通。
async fn check_tcp() -> bool {
    for (ip, port) in TCP_TARGETS {
        let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() else {
            continue;
        };
        if tokio::time::timeout(
            Duration::from_millis(1500),
            TcpStream::connect(addr),
        )
        .await
        .is_ok_and(|r| r.is_ok())
        {
            return true;
        }
    }
    false
}

/// 返回 `(整体是否在线, 网卡是否正常, ping 是否成功, TCP 是否成功)`。
///
/// 「是否在线」= 网卡正常 **且**（ping 通 **或** TCP 通）。
///
/// 原来只返回两个值，把真实的 ping 结果丢掉了，前端拿到的 `ping_success`
/// 其实是"整体是否在线"，UI 上 "Ping" 那一行显示的语义是错的（P1-4）。
///
/// ping 与 TCP **并发**而不是串行：在线时第一个 ping 目标通常就成功（只 fork 一个
/// 进程，开销与原来持平），而并发让离线路径的最坏耗时从「3×1s + 1.5s」压到 1.5s，
/// 不至于把 5 秒的检测周期拖成 8 秒。
async fn check_network_status() -> (bool, bool, bool, bool) {
    let adapter_ok = check_adapter_status();
    let (ping_ok, tcp_ok) = tokio::join!(check_ping(), check_tcp());
    let reachable = ping_ok || tcp_ok;
    (adapter_ok && reachable, adapter_ok, ping_ok, tcp_ok)
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
fn set_autostart(enable: bool) -> Result<(), String> {
    // 原来无条件 `Ok(())`：界面上勾选「开机启动」会显示保存成功，实际什么都没发生，
    // 是一个静默的假开关。如实报错，让界面把这个选项置灰并说明原因。
    if enable {
        Err("autostart is only implemented on Windows".to_string())
    } else {
        Ok(())
    }
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
        .map_err(std::io::Error::other)?;
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
        // 托盘菜单也要跟着换语言，否则用户在托盘里切完语言、看到的还是旧文案（P1-8）。
        refresh_tray_menu(app);
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

/// 界面上用得上的运行时信息。
#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    /// `std::env::consts::OS`，即 `windows` / `macos` / `linux`。
    pub platform: &'static str,
    /// 开机自启是否真的实现了。界面上要把这个选项置灰，
    /// 而不是让用户勾了之后"保存成功"却什么都不发生。
    pub autostart_supported: bool,
}

/// 运行时信息。Tauri 命令和 HTTP 路由共用（`#[tauri::command]` 会把函数包一层，
/// 包过之后就没法直接当 axum handler 用了）。
fn app_info() -> AppInfo {
    AppInfo {
        platform: std::env::consts::OS,
        autostart_supported: cfg!(target_os = "windows"),
    }
}

#[tauri::command]
fn get_app_info() -> AppInfo {
    app_info()
}

/// Web 服务运行状态。
///
/// 启动阶段 bind 失败只发过 `web-server-error` 事件，而事件早于前端监听注册会直接
/// 丢失——窗口又是隐藏的，用户什么都看不到。界面改为启动后**主动查一次**（P1-5）。
#[tauri::command]
fn get_web_status(state: tauri::State<'_, Arc<AppState>>) -> WebStatusInfo {
    let status = state.web_status.lock().unwrap().clone();
    let lang = state.settings.lock().unwrap().language.clone();
    status.info(&state.i18n, &lang)
}

// ───────────────────────────── 内嵌 Web 服务 ─────────────────────────────

/// 百分号解码。
///
/// 单独抽出来是因为顺序很重要：`uri().path()` 返回的是**未经解码**的原始路径，
/// `/%2e%2e/x` 的首段是 `%2e%2e` 而不是 `..`，能顺利通过字面检查；如果下游的
/// 资源解析器再解码一次，它就重新变回了 `..`。原实现等于把安全性押在
/// 「下游绝不会二次解码」这个假设上。现在改成先解码、再检查。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把请求路径映射成资源路径。
///
/// 返回 `None` 表示这个路径**不允许**访问（P1-2 路径穿越防护）：
/// 任何 `..`、`.`、反斜杠、盘符都被拒绝，保证绝不会读到前端资源目录之外的文件。
/// 原来直接 `dist_path.join(path.trim_start_matches('/'))`，
/// 请求 `/../../Windows/win.ini` 就能读到任意文件。
fn sanitize_asset_path(raw_path: &str) -> Option<String> {
    // 先从**原始串**上摘掉 query / fragment：`?` 和 `#` 不是合法的百分号编码字符，
    // 必须先切后解码，否则 `%3F` 会被解码成 `?` 而被误当成查询串分隔符。
    let raw = raw_path
        .trim_start_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("");

    if raw.is_empty() {
        return Some("index.html".to_string());
    }

    // 再解码，按解码后的真实路径做穿越检查。
    let decoded = percent_decode(raw);

    let mut segments: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
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
/// 会先给旧的监听发关闭信号，然后**等它真正退出**（端口 / 套接字释放）再在新端口上 bind。
/// 原来发完信号就立刻 spawn 新任务，中间没有任何同步，而旧服务此刻还在处理触发本次
/// 变更的 `/api/save-settings` 请求、socket 要到响应刷出才关闭。切换"允许局域网访问"
/// 时端口并不变（`127.0.0.1:P` → `0.0.0.0:P`），正好撞上这个窗口：Windows 上 Rust std
/// 不设 `SO_REUSEADDR`，直接 `WSAEADDRINUSE`——而 Windows 是唯一发布平台（P1-6）。
///
/// bind 失败会重试若干次；全部失败则记日志并把 `WebStatus` 置为 `Failed`。
/// 界面不再依赖事件通知（启动阶段的 emit 早于前端监听注册会直接丢失），
/// 而是主动查询这个状态（P1-5）。
fn start_web_server(app: AppHandle, state: Arc<AppState>, port: u16) {
    if let Some(tx) = state.server_shutdown.lock().unwrap().take() {
        let _ = tx.send(());
    }
    // 把旧任务摘出来，等它退出后再 bind。
    let previous = state.server_task.lock().unwrap().take();
    *state.web_status.lock().unwrap() = WebStatus::Starting;

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    *state.server_shutdown.lock().unwrap() = Some(tx);

    let web_access = state.settings.lock().unwrap().web_access;

    // 复用 Tauri 自带的 tokio runtime（P2-10），不再为每个任务各建一个 Runtime。
    let handle = tauri::async_runtime::spawn({
        // 移进异步任务的是克隆体，外层的 `state` 还要用来回填 server_task 句柄。
        let state = state.clone();
        async move {
        // 等旧服务真正退出，套接字释放后再 bind。
        if let Some(prev) = previous {
            let _ = prev.await;
        }

        let s_status = state.clone();
        let s_settings = state.clone();
        let s_save = state.clone();
        let s_logs = state.clone();
        let s_web = state.clone();

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
                            Ok(_) => {
                                // 把新地址一并返回。改端口 / 改访问范围会让当前页面
                                // 停在失效的 origin 上，之前是静默失败，没有任何提示。
                                let port = s.settings.lock().unwrap().port;
                                (
                                    StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "success": true,
                                        "url": format!("http://127.0.0.1:{port}"),
                                    })),
                                )
                                    .into_response()
                            }
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
            // Web 服务自身的运行状态。启动阶段的事件会丢，界面靠这个端点兜底。
            .route(
                "/api/web-status",
                axum::routing::get(move || {
                    let s = s_web.clone();
                    async move {
                        let st = s.web_status.lock().unwrap().clone();
                        let lang = s.settings.lock().unwrap().language.clone();
                        axum::Json(st.info(&s.i18n, &lang))
                    }
                }),
            )
            // 运行时信息：界面据此把未实现的选项（如非 Windows 的开机自启）置灰。
            .route(
                "/api/app-info",
                axum::routing::get(|| async { axum::Json(app_info()) }),
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

        // bind 本身也可能因为端口被别的进程抢占而失败，重试几次再放弃。
        let mut listener = None;
        let mut last_err = String::new();
        for attempt in 1..=BIND_RETRIES {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => {
                    listener = Some(l);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    write_log(
                        "WARN",
                        "log_bind_retry",
                        Some(&format!("{attempt}/{BIND_RETRIES} {addr}: {e}")),
                    );
                    if attempt < BIND_RETRIES {
                        sleep(Duration::from_millis(
                            BIND_RETRY_DELAY_MS * attempt as u64,
                        ))
                        .await;
                    }
                }
            }
        }

        let Some(listener) = listener else {
            let detail = format!("{addr}: {last_err}");
            write_log("ERROR", "log_bind_failed", Some(&detail));
            *state.web_status.lock().unwrap() = WebStatus::Failed(detail.clone());
            let _ = app_for_emit.emit("web-server-error", detail);
            return;
        };

        *state.web_status.lock().unwrap() = WebStatus::Running(addr.to_string());
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
            Ok(()) => {
                write_log("INFO", "log_web_stopped", Some(&addr.to_string()));
                *state.web_status.lock().unwrap() = WebStatus::Stopped;
            }
            Err(e) => {
                // 原来错误也用 `log_web_stopped`，日志面板里显示"Web 服务已停止"，
                // 会把真实原因彻底掩盖掉。
                write_log("ERROR", "log_web_error", Some(&e.to_string()));
                *state.web_status.lock().unwrap() = WebStatus::Failed(e.to_string());
            }
        }
        }
    });

    *state.server_task.lock().unwrap() = Some(handle);
}

// ───────────────────────────── 网络监控循环 ─────────────────────────────

/// 从记录里的 `start` 文本反推起始 epoch 秒。
///
/// 升级之前写入的 `history.json` 没有 `start_epoch` 字段，只有一个给人看的格式化
/// 字符串。靠它把"断网中途升级/重启"的那一段时长补齐。格式是代码自己写死的
/// `%Y-%m-%d %H:%M:%S`，所以可以稳定解析回来。
fn epoch_from_start_text(start: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|dt| dt.and_local_timezone(Local).single())
        .map(|dt| dt.timestamp())
}

/// 发一条系统通知（P0-2）。
///
/// 断网是「用户不看屏幕时才会注意到」的事件，只把已打开的窗口变红等于
/// 把告警做在了唯一不会被看到的地方。一段断网只发一次、恢复时再发一条，
/// 所以网络抖动不会刷屏。
fn notify(app: &AppHandle, title: String, body: String) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        // 通知失败不能影响监控本身，只留一条日志。
        write_log("ERROR", "log_notify_failed", Some(&e.to_string()));
    }
}

fn start_network_monitoring(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = interval(Duration::from_secs(CHECK_INTERVAL));
        // 检测本身有可能超过 5 秒；默认的 Burst 会一次性补发多个 tick，
        // 造成日志刷屏。改成 Delay：错过就错过，下一轮从当前时刻重新计（P2-9）。
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            // 先把需要 await 的网络检测跑完，再去拿锁，
            // 避免把 `std::sync::Mutex` 的 guard 跨 await 持有。
            let (is_online, adapter_ok, ping_ok, tcp_ok) = check_network_status().await;
            let now_local = Local::now();
            let now = now_local.format("%Y-%m-%d %H:%M:%S").to_string();
            let now_epoch = now_local.timestamp();

            let went_offline;
            let came_online;
            let mut opened_this_tick: Option<i64> = None;
            let mut recovered_after: Option<u64> = None;

            {
                let mut status = state.status.lock().unwrap();
                let old_online = status.is_online;

                status.is_online = is_online;
                status.adapter_status = adapter_ok;
                status.ping_success = ping_ok;
                status.tcp_success = tcp_ok;
                status.last_check = now.clone();

                went_offline = !is_online && old_online;
                came_online = is_online && !old_online;

                if went_offline {
                    // 起点已经存在 = 这段断网是跨重启续接的：只推进时长，
                    // **不**新增记录，否则每重启一次就多一条重复记录。
                    let mut slot = state.outage_start_epoch.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(now_epoch);
                        status.disconnect_history.push(DisconnectRecord {
                            start: now.clone(),
                            end: None,
                            duration_secs: None,
                            start_epoch: Some(now_epoch),
                            end_epoch: None,
                        });

                        let overflow = status.disconnect_history.len().saturating_sub(MAX_HISTORY);
                        if overflow > 0 {
                            status.disconnect_history.drain(0..overflow);
                        }

                        status.disconnect_count = status.disconnect_count.saturating_add(1);
                        opened_this_tick = Some(now_epoch);
                    }
                    // 刚断开的那一刻就是 0 秒，而不是 None（P2-18 的前端侧对应问题）
                    status.current_disconnect_duration =
                        slot.map(|s| (now_epoch - s).max(0) as u64);
                } else if came_online {
                    if let Some(last) = status.disconnect_history.last_mut() {
                        // 只闭合仍然是"开放"的那条，别把上一段已结束的断网改写掉。
                        if last.end.is_none() {
                            let start = state
                                .outage_start_epoch
                                .lock()
                                .unwrap()
                                .take()
                                .or_else(|| epoch_from_start_text(&last.start));
                            let duration = start.map(|s| (now_epoch - s).max(0) as u64);
                            // 连起点都推不出来（数据损坏）时也照样闭合，
                            // 界面才不会永远停在"仍在断网中"。
                            last.end = Some(now.clone());
                            last.end_epoch = Some(now_epoch);
                            last.duration_secs = duration.or(Some(0));
                            recovered_after = duration;
                        }
                    } else {
                        // 没有历史可闭合时，别把起点留在状态里。
                        state.outage_start_epoch.lock().unwrap().take();
                    }
                    // 恢复后必须清零（P1-9），否则统计卡片永远停在上一次的秒数
                    status.current_disconnect_duration = None;
                } else if !is_online {
                    // 持续断网中：记录保持开放，只推进已持续时长。
                    let slot = state.outage_start_epoch.lock().unwrap();
                    status.current_disconnect_duration =
                        slot.map(|s| (now_epoch - s).max(0) as u64);
                }
            }
            // ── 锁已释放，下面再做日志 / 通知 / 落盘这类 IO（P2-11）──

            let lang = state.settings.lock().unwrap().language.clone();

            if opened_this_tick.is_some() {
                write_log("WARN", "log_disconnected", None);
                notify(
                    &app,
                    state.i18n.t(&lang, "notify_offline_title"),
                    state.i18n.t(&lang, "notify_offline_body"),
                );
                persist_history(&state);
            } else if came_online {
                // 有真实断网时长才通知"已恢复"，避免启动瞬间的伪恢复也弹一条。
                if let Some(duration) = recovered_after {
                    write_log("INFO", "log_restored", Some(&format!("{duration}s")));
                    notify(
                        &app,
                        state.i18n.t(&lang, "notify_restored_title"),
                        state.i18n.t(&lang, "notify_restored_body"),
                    );
                }
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
        // 原来用的是 `log_settings_saved`，日志面板会显示"设置已保存 | <错误信息>"，
        // 语义完全相反，排障时会被直接误导。
        write_log("ERROR", "log_settings_save_failed", Some(&e));
    }
}

/// 按给定语言构建托盘菜单。
fn build_tray_menu(app: &AppHandle, lang: &str) -> tauri::Result<Menu<tauri::Wry>> {
    let state = app.state::<Arc<AppState>>();
    let i18n = &state.i18n;

    let settings_item = MenuItem::with_id(app, "settings", i18n.t(lang, "settings"), true, None::<&str>)?;
    let logs_item = MenuItem::with_id(app, "logs", i18n.t(lang, "view_logs"), true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let lang_zh_item = MenuItem::with_id(app, "lang_zh", "中文", true, None::<&str>)?;
    let lang_en_item = MenuItem::with_id(app, "lang_en", "English", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let about_item = MenuItem::with_id(app, "about", i18n.t(lang, "about"), true, None::<&str>)?;
    // 退出项也走 i18n（P2-14），不再硬编码 "Quit"
    let quit_item = MenuItem::with_id(app, "quit", i18n.t(lang, "quit"), true, None::<&str>)?;

    Menu::with_items(
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
    )
}

/// 语言切换后重建托盘菜单（P1-8）。
///
/// 托盘菜单原来只在启动时按当时的语言翻译一次：用户在托盘里点 English 之后，
/// 界面更新了、托盘菜单还是中文，必须重启进程——而"切语言"这个入口本身就在
/// 托盘菜单里，反馈直接错位。
fn refresh_tray_menu(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let lang = state.settings.lock().unwrap().language.clone();
    let menu = match build_tray_menu(app, &lang) {
        Ok(m) => m,
        Err(e) => {
            write_log("ERROR", "log_tray_failed", Some(&e.to_string()));
            return;
        }
    };
    let tray = state.tray.lock().unwrap();
    if let Some(tray) = tray.as_ref() {
        if let Err(e) = tray.set_menu(Some(menu)) {
            write_log("ERROR", "log_tray_failed", Some(&e.to_string()));
        }
    }
}

fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let lang = app.state::<Arc<AppState>>().settings.lock().unwrap().language.clone();
    let menu = build_tray_menu(app, &lang)?;

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("NetSentry")
        .show_menu_on_left_click(false);

    // 没有图标时不要 panic：托盘图标缺失只影响观感，不该让整个启动流程失败。
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    let tray = builder
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
        .build(app)?;

    // 存下句柄。托盘图标本身由 app 的 tray manager 按 id 持有，drop 不会让图标消失，
    // 但语言切换时需要用它 `set_menu` 重建菜单。`TrayIcon` 是 Send + Sync，可以安全共享。
    *app.state::<Arc<AppState>>().tray.lock().unwrap() = Some(tray);

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

    // ── 跨重启续接断网状态（P0-1）──
    //
    // 如果上一轮是在断网中被中断的（进程被杀、系统更新、手动退出），history.json 里
    // 最后一条记录仍然是"开放"的（`end: None`）。这里做两件事：
    //   1. 把它的起点接过来，本次运行持续断网时时长能接着往上加；
    //   2. 把初始在线态置为**离线**。
    //
    // 第 2 点是关键。原来这里无条件写 `is_online: true`，于是重启后首轮探测无论
    // 结果如何都不会产生跃迁，磁盘上那条开放记录永远没人补，界面就一直显示
    // "仍在断网中"。置为离线后，首轮要么闭合它（网络已恢复），要么正确地延续它
    // （网络仍断），两种情况都自洽。
    let open_record = persisted
        .disconnect_history
        .last()
        .filter(|r| r.end.is_none());
    let resumed_outage = open_record
        .and_then(|r| r.start_epoch)
        .or_else(|| open_record.and_then(|r| epoch_from_start_text(&r.start)));

    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        status: Mutex::new(NetworkStatus {
            is_online: open_record.is_none(),
            adapter_status: true,
            ping_success: true,
            tcp_success: true,
            last_check: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            disconnect_count: persisted.disconnect_count,
            current_disconnect_duration: resumed_outage.map(|_| 0),
            disconnect_history: persisted.disconnect_history,
        }),
        i18n: I18n::load(),
        server_shutdown: Mutex::new(None),
        server_task: Mutex::new(None),
        web_status: Mutex::new(WebStatus::Stopped),
        token: load_or_create_token(),
        outage_start_epoch: Mutex::new(resumed_outage),
        tray: Mutex::new(None),
    });

    tauri::Builder::default()
        // 单实例保护**必须注册在第一个**（P1-7）。没有它，多开会同时跑两个监控循环：
        // 并发写同一个日志文件（`LOG_LOCK` 只在进程内有效，跨进程会撕裂行）、
        // 争抢同一端口（第二个静默失败）、互相覆盖 history.json 造成统计丢失。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二个实例只负责把已有实例的窗口叫出来，然后自己退出。
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        // 断网/恢复的系统通知（P0-2）
        .plugin(tauri_plugin_notification::init())
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
            get_web_status,
            get_app_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
