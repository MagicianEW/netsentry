use axum::response::IntoResponse;
use chrono::Local;
use log::{error, info, warn, LevelFilter};
use network_interface::{NetworkInterface as _, NetworkInterfaceConfig};
use serde::{Deserialize, Serialize};
use simplelog::{CombinedLogger, Config, WriteLogger};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tokio::time::interval;
use std::net::SocketAddr;
use std::borrow::Cow;
use axum::body;

const LOG_SIZE_LIMIT: u64 = 128 * 1024;
const PING_TARGETS: &[&str] = &["8.8.8.8", "1.1.1.1", "114.114.114.114"];
const CHECK_INTERVAL: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub language: String,
    pub port: u16,
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: "zh".to_string(),
            port: 8080,
            autostart: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub is_online: bool,
    pub adapter_status: bool,
    pub ping_success: bool,
    pub last_check: String,
    pub disconnect_count: u32,
    pub current_disconnect_duration: Option<u64>,
    pub disconnect_history: Vec<DisconnectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisconnectRecord {
    pub start: String,
    pub end: Option<String>,
    pub duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
    pub translated: String,
}

pub struct AppState {
    pub settings: Mutex<Settings>,
    pub status: Mutex<NetworkStatus>,
    pub log_file: Mutex<PathBuf>,
    pub disconnect_start: Mutex<Option<Instant>>,
    pub i18n: I18n,
}

#[derive(Clone)]
pub struct I18n {
    pub dict: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl I18n {
    pub fn new(_lang: &str) -> Self {
        let mut dict = std::collections::HashMap::new();

        let mut zh = std::collections::HashMap::new();
        zh.insert("app_title".into(), "网络监控".into());
        zh.insert("online".into(), "在线".into());
        zh.insert("offline".into(), "离线".into());
        zh.insert("checking".into(), "检测中...".into());
        zh.insert("settings".into(), "设置".into());
        zh.insert("language".into(), "语言".into());
        zh.insert("port".into(), "端口".into());
        zh.insert("autostart".into(), "开机启动".into());
        zh.insert("save".into(), "保存".into());
        zh.insert("cancel".into(), "取消".into());
        zh.insert("disconnect_warning".into(), "网络已断开！".into());
        zh.insert("disconnect_duration".into(), "断网持续时间".into());
        zh.insert("disconnect_count".into(), "断网次数".into());
        zh.insert("disconnect_history".into(), "断网记录".into());
        zh.insert("still_offline".into(), "仍处于离线状态".into());
        zh.insert("restored".into(), "网络已恢复".into());
        zh.insert("adapter_normal".into(), "适配器正常".into());
        zh.insert("adapter_error".into(), "适配器异常".into());
        zh.insert("ping_normal".into(), "Ping正常".into());
        zh.insert("ping_failed".into(), "Ping失败".into());
        zh.insert("view_logs".into(), "查看日志".into());
        zh.insert("clear_logs".into(), "清空日志".into());
        zh.insert("about".into(), "关于".into());
        zh.insert("version".into(), "版本".into());
        zh.insert("network_status".into(), "网络状态".into());
        zh.insert("connection_info".into(), "连接信息".into());
        zh.insert("settings_saved".into(), "设置已保存".into());
        zh.insert("seconds".into(), "秒".into());
        zh.insert("logs".into(), "日志".into());
        zh.insert("no_logs".into(), "暂无日志".into());
        zh.insert("language_changed".into(), "语言已切换".into());
        zh.insert("port_in_use".into(), "端口已被占用".into());
        zh.insert("autostart_enabled".into(), "已启用开机启动".into());
        zh.insert("autostart_disabled".into(), "已禁用开机启动".into());
        dict.insert("zh".into(), zh);

        let mut en = std::collections::HashMap::new();
        en.insert("app_title".into(), "Network Monitor".into());
        en.insert("online".into(), "Online".into());
        en.insert("offline".into(), "Offline".into());
        en.insert("checking".into(), "Checking...".into());
        en.insert("settings".into(), "Settings".into());
        en.insert("language".into(), "Language".into());
        en.insert("port".into(), "Port".into());
        en.insert("autostart".into(), "Autostart".into());
        en.insert("save".into(), "Save".into());
        en.insert("cancel".into(), "Cancel".into());
        en.insert("disconnect_warning".into(), "Network Disconnected!".into());
        en.insert("disconnect_duration".into(), "Disconnect Duration".into());
        en.insert("disconnect_count".into(), "Disconnect Count".into());
        en.insert("disconnect_history".into(), "Disconnect History".into());
        en.insert("still_offline".into(), "Still offline".into());
        en.insert("restored".into(), "Network Restored".into());
        en.insert("adapter_normal".into(), "Normal".into());
        en.insert("adapter_error".into(), "Error".into());
        en.insert("ping_normal".into(), "OK".into());
        en.insert("ping_failed".into(), "Failed".into());
        en.insert("view_logs".into(), "View Logs".into());
        en.insert("clear_logs".into(), "Clear Logs".into());
        en.insert("about".into(), "About".into());
        en.insert("version".into(), "Version".into());
        en.insert("network_status".into(), "Network Status".into());
        en.insert("connection_info".into(), "Connection Info".into());
        en.insert("settings_saved".into(), "Settings Saved".into());
        en.insert("seconds".into(), "seconds".into());
        en.insert("logs".into(), "Logs".into());
        en.insert("no_logs".into(), "No logs".into());
        en.insert("language_changed".into(), "Language Changed".into());
        en.insert("port_in_use".into(), "Port in use".into());
        en.insert("autostart_enabled".into(), "Autostart Enabled".into());
        en.insert("autostart_disabled".into(), "Autostart Disabled".into());
        dict.insert("en".into(), en);

        Self { dict }
    }

    pub fn t_lang(&self, lang: &str, key: &str) -> String {
        self.dict
            .get(lang)
            .and_then(|d| d.get(key))
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
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
            let archive_name = format!("netsentry_{}.log", timestamp);
            let archive_path = get_log_dir().join(archive_name);
            fs::rename(log_path, &archive_path)?;
        }
    }
    Ok(log_path.clone())
}

fn setup_logging() -> std::io::Result<()> {
    let log_path = get_current_log_file()?;
    let rotated_path = rotate_log_if_needed(&log_path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rotated_path)?;

    CombinedLogger::init(vec![WriteLogger::new(
        LevelFilter::Info,
        Config::default(),
        file,
    )])
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    Ok(())
}

fn write_log(level: &str, msg_en: &str, msg_zh: String, state: &AppState) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let settings = state.settings.lock().unwrap();

    let log_file_path = get_current_log_file().unwrap_or_else(|_| PathBuf::from("netsentry.log"));
    let rotated_path = rotate_log_if_needed(&log_file_path).unwrap_or(log_file_path);

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&rotated_path) {
        let lang = if settings.language == "en" { "EN" } else { "ZH" };
        let formatted = format!("[{}] [{}] [{}] {}\n", timestamp, level, lang, msg_en);
        let _ = file.write_all(formatted.as_bytes());
    }

    drop(settings);

    match level {
        "ERROR" => error!("{}", msg_en),
        "WARN" => warn!("{}", msg_en),
        _ => info!("{}", msg_en),
    }
}

fn check_adapter_status() -> bool {
    match network_interface::NetworkInterface::show() {
        Ok(interfaces) => {
            for iface in interfaces {
                if iface.name.contains("Ethernet")
                    || iface.name.contains("Wi-Fi")
                    || iface.name.contains("WLAN")
                    || iface.name.contains("Local")
                {
                    if !iface.addr.is_empty() {
                        return true;
                    }
                }
            }
            for iface in interfaces {
                if iface.flags.contains(network_interface::NetworkInterfaceType::Broadcast) {
                    if !iface.addr.is_empty() {
                        return true;
                    }
                }
            }
            false
        }
        Err(_) => false,
    }
}

fn check_ping() -> bool {
    for target in PING_TARGETS {
        let output = Command::new("ping")
            .args(["-n", "1", "-w", "1000", target])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                return true;
            }
        }
    }
    false
}

fn check_network_status() -> (bool, bool) {
    let adapter_ok = check_adapter_status();
    let ping_ok = check_ping();
    let is_online = adapter_ok && ping_ok;
    (is_online, adapter_ok)
}

#[cfg(target_os = "windows")]
fn set_autostart(enable: bool) -> Result<(), String> {
    use winreg::enums::*;
    use winreg::RegKey;

    let exe_path = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .to_string();

    let key_path = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    let reg_key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(key_path, KEY_SET_VALUE)
        .map_err(|e| e.to_string())?;

    if enable {
        reg_key.set_value("NetSentry", &exe_path).map_err(|e| e.to_string())?;
    } else {
        let _ = reg_key.delete_value("NetSentry");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn set_autostart(_enable: bool) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(settings: Settings, state: tauri::State<'_, Arc<AppState>>, app: AppHandle) -> Result<String, String> {
    let old_settings = state.settings.lock().unwrap().clone();

    *state.settings.lock().unwrap() = settings.clone();

    let i18n = &state.i18n;

    if old_settings.autostart != settings.autostart {
        if let Err(e) = set_autostart(settings.autostart) {
            return Err(e);
        }
        let msg = if settings.autostart {
            i18n.t_lang(&settings.language, "autostart_enabled")
        } else {
            i18n.t_lang(&settings.language, "autostart_disabled")
        };
        write_log("INFO", &format!("Autostart {}", if settings.autostart { "enabled" } else { "disabled" }), msg, &state);
    }

    if old_settings.language != settings.language {
        let msg = i18n.t_lang(&settings.language, "language_changed");
        write_log("INFO", &format!("Language changed to {}", settings.language), msg, &state);
    }

    write_log("INFO", "Settings saved", i18n.t_lang(&settings.language, "settings_saved"), &state);

    let _ = app.emit("settings-changed", settings.clone());

    Ok(i18n.t_lang(&settings.language, "settings_saved"))
}

#[tauri::command]
fn get_status(state: tauri::State<'_, Arc<AppState>>) -> NetworkStatus {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn get_translated_logs(state: tauri::State<'_, Arc<AppState>>) -> Vec<LogEntry> {
    let lang = state.settings.lock().unwrap().language.clone();
    let i18n = &state.i18n;
    let log_path = get_current_log_file().unwrap_or_else(|_| PathBuf::from("netsentry.log"));

    let mut entries = Vec::new();

    if let Ok(file) = File::open(&log_path) {
        let reader = BufReader::new(file);
        for line in reader.lines().take(500) {
            if let Ok(line) = line {
                let parts: Vec<&str> = line.splitn(5, ' ').collect();
                if parts.len() >= 4 {
                    let level = parts[2].trim_matches(|c| c == '[' || c == ']').to_string();
                    let message = parts.get(4).unwrap_or(&"").to_string();
                    entries.push(LogEntry {
                        timestamp: parts.get(0).unwrap_or(&"").to_string(),
                        level: level.clone(),
                        message: message.clone(),
                        translated: i18n.t_lang(&lang, &message),
                    });
                }
            }
        }
    }

    entries.reverse();
    entries
}

#[tauri::command]
fn get_logs(state: tauri::State<'_, Arc<AppState>>) -> Result<String, String> {
    let log_path = get_current_log_file().map_err(|e| e.to_string())?;
    fs::read_to_string(&log_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn show_window(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn start_network_monitoring(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut ticker = interval(tokio::time::Duration::from_secs(CHECK_INTERVAL));
            let mut disconnect_start_time: Option<Instant> = None;

            loop {
                ticker.tick().await;

                let (is_online, adapter_ok) = check_network_status();
                let ping_ok = is_online;
                let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

                let mut status = state.status.lock().unwrap();
                let old_online = status.is_online;
                status.is_online = is_online;
                status.last_check = now;
                status.adapter_status = adapter_ok;
                status.ping_success = ping_ok;

                let settings = state.settings.lock().unwrap();
                let lang = settings.language.clone();
                drop(settings);

                if !is_online && old_online {
                    write_log("WARN", "Network disconnected", state.i18n.t_lang(&lang, "disconnect_warning"), &state);

                    if disconnect_start_time.is_none() {
                        disconnect_start_time = Some(Instant::now());
                    }

                    let record = DisconnectRecord {
                        start: now.clone(),
                        end: None,
                        duration_secs: None,
                    };
                    status.disconnect_history.push(record);
                    status.disconnect_count += 1;

                    let _ = app.emit("network-offline", status.clone());
                } else if is_online && !old_online {
                    write_log("INFO", "Network restored", state.i18n.t_lang(&lang, "restored"), &state);

                    if let Some(start) = disconnect_start_time.take() {
                        let duration = start.elapsed().as_secs();
                        if let Some(last) = status.disconnect_history.last_mut() {
                            last.end = Some(now.clone());
                            last.duration_secs = Some(duration);
                        }
                    }

                    let _ = app.emit("network-online", status.clone());
                }

                if !is_online {
                    if let Some(start) = &disconnect_start_time {
                        status.current_disconnect_duration = Some(start.elapsed().as_secs());
                    }
                    let _ = app.emit("network-status-update", status.clone());
                }

                drop(status);
            }
        });
    });
}

fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let state = app.state::<Arc<AppState>>();
    let settings = state.settings.lock().unwrap();
    let lang = settings.language.clone();
    drop(settings);

    let i18n = &state.i18n;

    let show_item = MenuItem::with_id(app, "show", i18n.t_lang(&lang, "settings"), true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app, "settings", i18n.t_lang(&lang, "settings"), true, None::<&str>)?;
    let logs_item = MenuItem::with_id(app, "logs", i18n.t_lang(&lang, "view_logs"), true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let lang_zh_item = MenuItem::with_id(app, "lang_zh", "中文", true, None::<&str>)?;
    let lang_en_item = MenuItem::with_id(app, "lang_en", "English", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let about_item = MenuItem::with_id(app, "about", i18n.t_lang(&lang, "about"), true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_item,
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

    let _tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("NetSentry")
        .icon(app.default_window_icon().unwrap().clone())
        .menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let state = app.state::<Arc<AppState>>();
            let i18n = &state.i18n;

            match event.id.as_ref() {
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "settings" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = app.emit("open-settings", ());
                    }
                }
                "logs" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = app.emit("open-logs", ());
                    }
                }
                "lang_zh" => {
                    let mut settings = state.settings.lock().unwrap();
                    settings.language = "zh".to_string();
                    drop(settings);
                    let _ = app.emit("settings-changed", state.settings.lock().unwrap().clone());
                    write_log("INFO", "Language changed to Chinese", i18n.t_lang("zh", "language_changed"), &state);
                    let _ = app.emit("language-changed", "zh");
                }
                "lang_en" => {
                    let mut settings = state.settings.lock().unwrap();
                    settings.language = "en".to_string();
                    drop(settings);
                    let _ = app.emit("settings-changed", state.settings.lock().unwrap().clone());
                    write_log("INFO", "Language changed to English", i18n.t_lang("en", "language_changed"), &state);
                    let _ = app.emit("language-changed", "en");
                }
                "about" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = app.emit("open-about", ());
                    }
                }
                "quit" => {
                    write_log("INFO", "Application exiting", "应用程序退出", &state);
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}

fn load_settings() -> Settings {
    let config_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("NetSentry")
        .join("settings.json");

    if let Ok(data) = fs::read_to_string(&config_path) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Settings::default()
    }
}

fn save_settings_to_file(settings: &Settings) -> std::io::Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("NetSentry");

    fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("settings.json");

    let json = serde_json::to_string_pretty(settings).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(config_path, json)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = ensure_log_dir();
    let _ = setup_logging();

    let settings = load_settings();
    let settings_clone = settings.clone();
    let _ = save_settings_to_file(&settings);

    let state = Arc::new(AppState {
        settings: Mutex::new(settings),
        status: Mutex::new(NetworkStatus {
            is_online: true,
            adapter_status: true,
            ping_success: true,
            last_check: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            disconnect_count: 0,
            current_disconnect_duration: None,
            disconnect_history: Vec::new(),
        }),
        log_file: Mutex::new(get_current_log_file().unwrap_or_default()),
        disconnect_start: Mutex::new(None),
        i18n: I18n::new(&settings_clone.language),
    });

    info!("NetSentry starting...");

    let port = settings_clone.port;

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .setup(move |app| {
            let state_arc = (*app.state::<Arc<AppState>>().inner()).clone();
            let port = port;

            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let state = state_arc.clone();

                    let router = axum::Router::new()
                        .route("/api/status", axum::routing::get(move || {
                            let s = state.clone();
                            async move {
                                let status = s.status.lock().unwrap().clone();
                                axum::Json(serde_json::json!({ "status": status }))
                            }
                        }))
                        .route("/api/settings", axum::routing::get(move || {
                            let s = state.clone();
                            async move {
                                let settings = s.settings.lock().unwrap().clone();
                                axum::Json(settings)
                            }
                        }))
                        .route("/api/save-settings", axum::routing::post(move | axum::Json(settings): axum::Json<Settings>| {
                            let s = state.clone();
                            async move {
                                *s.settings.lock().unwrap() = settings.clone();
                                axum::Json(serde_json::json!({"success": true}))
                            }
                        }))
                        .route("/api/logs", axum::routing::get(move || {
                            let s = state.clone();
                            async move {
                                let logs_path = get_current_log_file().unwrap_or_default();
                                let logs_content = fs::read_to_string(&logs_path).unwrap_or_default();
                                axum::Json(serde_json::json!({ "logs": logs_content }))
                            }
                        }))
                        .fallback(|req: axum::http::Request<body::Body>| async move {
                            let path = req.uri().path();
                            let dist_path = std::path::PathBuf::from(std::env::current_dir().unwrap_or_default()).join("dist");

                            let file_path = if path == "/" {
                                dist_path.join("index.html")
                            } else {
                                dist_path.join(path.trim_start_matches('/'))
                            };

                            if file_path.exists() {
                                let content = fs::read(&file_path).unwrap_or_default();
                                let mime = if path.ends_with(".css") {
                                    "text/css"
                                } else if path.ends_with(".js") {
                                    "application/javascript"
                                } else if path.ends_with(".html") {
                                    "text/html"
                                } else {
                                    "text/plain"
                                };
                                axum::response::Response::builder()
                                    .header("Content-Type", mime)
                                    .body(axum::body::Body::from(content))
                                    .unwrap_or_else(|_| axum::response::Html("Not Found").into_response())
                            } else {
                                axum::response::Html("<html><body><h1>NetSentry</h1><p>Use Tauri window to configure. Web access requires building with dist folder.</p></body></html>").into_response()
                            }
                        });

                    let addr = SocketAddr::from(([0, 0, 0, 0], port));
                    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
                    tracing::info!("Web server listening on {}", addr);
                    axum::serve(listener, router).await.unwrap();
                });
            });

            if let Err(e) = setup_tray(app.handle()) {
                error!("Failed to setup tray: {}", e);
            }

            start_network_monitoring(app.handle().clone(), state_arc.clone());

            write_log("INFO", "NetSentry started successfully", "NetSentry 启动成功", &state_arc);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_status,
            get_translated_logs,
            get_logs,
            show_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
