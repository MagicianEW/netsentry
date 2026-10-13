import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import dict from "../i18n.json";

interface Settings {
  language: string;
  port: number;
  autostart: boolean;
  web_access: boolean;
}

interface NetworkStatus {
  is_online: boolean;
  adapter_status: boolean;
  ping_success: boolean;
  /** TCP 兜底探测结果。ping 被防火墙屏蔽时，网络可能仍然是通的。 */
  tcp_success: boolean;
  last_check: string;
  disconnect_count: number;
  current_disconnect_duration: number | null;
  disconnect_history: Array<{
    start: string;
    end: string | null;
    duration_secs: number | null;
  }>;
}

interface LogEntry {
  timestamp: string;
  level: string;
  message: string;
  translated: string;
}

interface WebStatusInfo {
  running: boolean;
  phase: "stopped" | "starting" | "running" | "failed";
  detail: string | null;
  /** 后端按当前语言拼好的一行摘要，界面直接显示。 */
  summary: string;
}

interface AppInfo {
  platform: string;
  autostart_supported: boolean;
}

/**
 * 语言资源直接复用仓库根目录的 `i18n.json`。
 *
 * 这份文件同时被 Rust 端 `include_str!` 读取，所以前端和后端**不可能**再出现
 * 字典不一致的情况（原来两边各写一份、已经漂移了）。
 */
const i18n = dict as unknown as Record<string, Record<string, string>>;

let currentLang = "zh";
let currentStatus: NetworkStatus | null = null;
/** 开机自启是否真的实现了。界面上据此置灰，而不是让用户勾了个寂寞。 */
let autostartSupported = true;

function t(key: string): string {
  return i18n[currentLang]?.[key] || i18n["en"]?.[key] || key;
}

/**
 * 取元素并显式断言存在。
 *
 * 原来的写法是 `document.getElementById("x")!`，一旦 id 打错或元素被删，
 * 只会在运行时报一个 "Cannot read properties of null"，很难定位；
 * 这里直接抛出带 id 的错误信息。
 */
function el<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) {
    throw new Error(`element #${id} is missing from the document`);
  }
  return node as T;
}

// ───────────────────────── 运行环境 ─────────────────────────

/**
 * 同一份前端资源有两种打开方式，界面逻辑完全共用，只有取数方式不同：
 *
 * 1. Tauri 窗口     —— 走 IPC（`invoke` + 事件推送）；
 * 2. 浏览器直接访问 —— 走内嵌 Web 服务的 HTTP 接口（`/api/*`）。
 */
type Mode = "tauri" | "web";
let mode: Mode | null = null;

function detectMode(): Mode {
  if (mode) return mode;
  const w = window as unknown as Record<string, unknown>;
  // `__TAURI_INTERNALS__` 由 WebView 的原生初始化脚本注入，一定存在；
  // `__TAURI__` 只有在 withGlobalTauri 打开时才有，这里一并兼容。
  mode =
    typeof w.__TAURI_INTERNALS__ !== "undefined" || typeof w.__TAURI__ !== "undefined"
      ? "tauri"
      : "web";
  return mode;
}

const TOKEN_KEY = "netsentry_token";

/** 浏览器访问时的令牌：优先取 URL 上的 ?token=，否则读记住的值。 */
function readToken(): string {
  const fromUrl = new URLSearchParams(location.search).get("token");
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl);
    // 读到之后立刻把令牌从地址栏抹掉。留在 URL 里会进浏览器历史，
    // 局域网内别人瞄一眼地址栏就能直接拿到局域网访问权限。
    window.history.replaceState(null, "", location.pathname);
    return fromUrl;
  }
  return localStorage.getItem(TOKEN_KEY) ?? "";
}

/**
 * 浏览器模式下调用内嵌 Web 服务。
 *
 * 令牌只对**非本机**请求生效：本机 127.0.0.1 访问不需要令牌，
 * 局域网访问才需要（见 README）。
 */
async function webFetch<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
  const token = readToken();
  const res = await fetch(path, {
    ...init,
    headers: {
      ...(init.headers ?? {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
  });

  if (res.status === 401 && retry) {
    const entered = window.prompt(t("token_prompt"));
    if (entered && entered.trim()) {
      localStorage.setItem(TOKEN_KEY, entered.trim());
      return webFetch<T>(path, init, false);
    }
    throw new Error(t("token_invalid"));
  }

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`);
  }

  const data: unknown = await res.json();
  if (data && typeof data === "object" && (data as { success?: boolean }).success === false) {
    throw new Error(String((data as { error?: string }).error ?? "request failed"));
  }
  return data as T;
}

const api = {
  async getSettings(): Promise<Settings> {
    return detectMode() === "tauri"
      ? invoke<Settings>("get_settings")
      : webFetch<Settings>("/api/settings");
  },

  async saveSettings(settings: Settings): Promise<unknown> {
    if (detectMode() === "tauri") {
      return invoke("save_settings", { settings });
    }
    return webFetch("/api/save-settings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(settings),
    });
  },

  async getStatus(): Promise<NetworkStatus> {
    if (detectMode() === "tauri") {
      return invoke<NetworkStatus>("get_status");
    }
    const payload = await webFetch<{ status: NetworkStatus }>("/api/status");
    return payload.status;
  },

  async getLogs(): Promise<LogEntry[]> {
    return detectMode() === "tauri"
      ? invoke<LogEntry[]>("get_translated_logs")
      : webFetch<LogEntry[]>("/api/logs");
  },

  /**
   * Web 服务运行状态。
   *
   * 后端在启动阶段只会 emit 一次 `web-server-error`，而那时前端监听器还没注册，
   * 事件会直接丢失；窗口又是隐藏的，端口被占用这类问题原本对用户完全不可见。
   * 所以启动后主动查一次。
   */
  async getWebStatus(): Promise<WebStatusInfo> {
    return detectMode() === "tauri"
      ? invoke<WebStatusInfo>("get_web_status")
      : webFetch<WebStatusInfo>("/api/web-status");
  },

  async getAppInfo(): Promise<AppInfo> {
    return detectMode() === "tauri"
      ? invoke<AppInfo>("get_app_info")
      : webFetch<AppInfo>("/api/app-info");
  },
};

// ───────────────────────── 渲染 ─────────────────────────

function updateTexts() {
  el("app-title").textContent = t("app_title");
  el("status-title").textContent = t("network_status");
  el("stats-title").textContent = t("statistics");
  el("history-title").textContent = t("disconnect_history");
  el("logs-title").textContent = t("logs");
  el("settings-modal-title").textContent = t("settings");
  el("about-modal-title").textContent = t("about");
  el("language-label").textContent = t("language");
  el("port-label").textContent = t("port");
  el("autostart-label").textContent = t("autostart");
  el("autostart-hint").textContent = t("autostart_unsupported");
  el("web-access-label").textContent = t("web_access");
  el("web-access-hint").textContent = t("web_access_hint");
  el("logs-btn").title = t("logs");
  el("save-settings-btn").textContent = t("save");
  el("cancel-settings-btn").textContent = t("cancel");
  el("disconnect-count-label").textContent = t("disconnect_count");
  el("current-duration-label").textContent = t("current_duration");
  el("last-check-label").textContent = t("last_check");
  el("adapter-label").textContent = t("adapter");
  el("ping-label").textContent = t("ping");
  el("tcp-label").textContent = t("tcp");
  el("web-status-label").textContent = t("web_status");
  el("refresh-logs-btn").title = t("refresh");
  el("about-version").textContent = `${t("version")} ${__APP_VERSION__}`;
  el("about-desc").textContent = t("app_desc");
  el("no-history").textContent = t("no_disconnect_history");

  updateStatusDisplay();
  updateHistoryDisplay();
}

/**
 * 把秒数渲染成 "12秒" / "12s"。
 *
 * 没有时长时显示破折号：原来的写法是 `` `-${t("seconds")}` ``，
 * 中文下会渲染成 "-秒"（P2-17）。
 */
function formatDuration(seconds: number | null): string {
  // 0 秒是合法值（刚断开的那一刻），所以只能用 === null 判断，不能用真值判断（P2-18）
  if (seconds === null || seconds === undefined) {
    return "—";
  }
  return `${seconds}${t("seconds")}`;
}

function updateStatusDisplay() {
  if (!currentStatus) return;

  const statusDot = el("status-dot");
  const statusText = el("status-text");
  const adapterStatus = el("adapter-status");
  const pingStatus = el("ping-status");
  const tcpStatus = el("tcp-status");
  const lastCheckTime = el("last-check-time");
  const disconnectCount = el("disconnect-count");
  const currentDuration = el("current-duration");
  const warningSection = el("warning-section");
  const warningDuration = el("warning-duration");

  if (currentStatus.is_online) {
    statusDot.className = "dot online";
    statusText.textContent = t("online");
    warningSection.classList.add("hidden");
  } else {
    statusDot.className = "dot offline";
    statusText.textContent = t("offline");
    warningSection.classList.remove("hidden");
    el("warning-title").textContent = t("disconnect_warning");
    warningDuration.textContent = `${t("disconnect_duration")}: ${formatDuration(
      currentStatus.current_disconnect_duration,
    )}`;
  }

  adapterStatus.textContent = currentStatus.adapter_status ? t("adapter_normal") : t("adapter_error");
  adapterStatus.style.color = currentStatus.adapter_status ? "#22c55e" : "#ef4444";

  pingStatus.textContent = currentStatus.ping_success ? t("ping_normal") : t("ping_failed");
  pingStatus.style.color = currentStatus.ping_success ? "#22c55e" : "#ef4444";

  // TCP 单独一行，如实展示。ping 失败但 TCP 通是**正常**状态（ICMP 被防火墙屏蔽），
  // 合并成一行或者反过来隐藏，都会让用户误以为整个网络有问题。
  tcpStatus.textContent = currentStatus.tcp_success ? t("tcp_normal") : t("tcp_failed");
  tcpStatus.style.color = currentStatus.tcp_success ? "#22c55e" : "#ef4444";

  lastCheckTime.textContent = currentStatus.last_check;
  disconnectCount.textContent = currentStatus.disconnect_count.toString();
  currentDuration.textContent = formatDuration(currentStatus.current_disconnect_duration);
}

function updateHistoryDisplay() {
  if (!currentStatus) return;

  const historyList = el("history-list");
  const noHistory = el("no-history");

  historyList.querySelectorAll(".history-item").forEach((node) => node.remove());

  if (currentStatus.disconnect_history.length === 0) {
    noHistory.classList.remove("hidden");
    return;
  }

  noHistory.classList.add("hidden");

  const records = [...currentStatus.disconnect_history].reverse().slice(0, 20);

  for (const record of records) {
    const item = document.createElement("div");
    item.className = "history-item";

    const timeSpan = document.createElement("span");
    timeSpan.className = "time";
    timeSpan.textContent = record.start;

    const durationSpan = document.createElement("span");
    durationSpan.className = "duration";
    durationSpan.textContent =
      record.end !== null && record.duration_secs !== null
        ? formatDuration(record.duration_secs)
        : t("still_offline");

    item.append(timeSpan, durationSpan);
    historyList.appendChild(item);
  }
}

// ───────────────────────── 取数 ─────────────────────────

async function loadSettings() {
  try {
    const settings = await api.getSettings();
    currentLang = settings.language;

    // 开机自启只在 Windows 上实现。其它平台上原后端直接返回 Ok，
    // 界面会显示"保存成功"却什么都不发生——这里直接置灰并说明原因。
    try {
      const info = await api.getAppInfo();
      autostartSupported = info.autostart_supported;
    } catch {
      // 拿不到就按支持处理，不因为一个辅助信息卡住整个设置面板。
      autostartSupported = true;
    }
    el<HTMLInputElement>("autostart-checkbox").disabled = !autostartSupported;
    el("autostart-hint").classList.toggle("hidden", autostartSupported);

    el<HTMLSelectElement>("language-select").value = settings.language;
    el<HTMLInputElement>("port-input").value = settings.port.toString();
    el<HTMLInputElement>("autostart-checkbox").checked = settings.autostart;
    el<HTMLInputElement>("web-access-checkbox").checked = settings.web_access;

    updateTexts();
  } catch (e) {
    console.error("Failed to load settings:", e);
  }
}

/**
 * 刷新 Web 服务状态。
 *
 * 后端在启动阶段只会 emit 一次 `web-server-error`，而那时前端监听器还没注册，
 * 事件直接丢失；窗口又是隐藏的，端口被占用这类问题原本对用户**完全不可见**。
 * 所以改成启动后主动查一次，并且把失败原因常驻显示。
 */
async function refreshWebStatus() {
  try {
    const info = await api.getWebStatus();
    el("web-status-value").textContent = info.summary;
    const banner = el("web-status-banner");
    if (info.phase === "failed") {
      el("web-status-text").textContent = info.summary;
      banner.classList.remove("hidden");
    } else {
      banner.classList.add("hidden");
    }
  } catch (e) {
    console.error("Failed to read web server status:", e);
  }
}

async function loadStatus() {
  try {
    currentStatus = await api.getStatus();
    updateStatusDisplay();
    updateHistoryDisplay();
  } catch (e) {
    console.error("Failed to load status:", e);
  }
}

async function loadLogs() {
  const logsContainer = el("logs-container");

  try {
    const logs = await api.getLogs();
    logsContainer.replaceChildren();

    if (logs.length === 0) {
      const empty = document.createElement("div");
      empty.className = "log-entry";
      const span = document.createElement("span");
      span.className = "timestamp";
      span.textContent = t("no_logs");
      empty.appendChild(span);
      logsContainer.appendChild(empty);
      return;
    }

    // 用 DOM 拼装而不是 innerHTML：日志内容虽然是自产的，但没有理由留注入面（P2-16）
    for (const log of logs.slice(0, 100)) {
      const entry = document.createElement("div");
      entry.className = "log-entry";

      const timestamp = document.createElement("span");
      timestamp.className = "timestamp";
      timestamp.textContent = log.timestamp;

      const level = document.createElement("span");
      level.className = `level ${log.level}`;
      level.textContent = log.level;

      const message = document.createElement("span");
      message.className = "message";
      message.textContent = log.translated;

      entry.append(timestamp, level, message);
      logsContainer.appendChild(entry);
    }
  } catch (e) {
    console.error("Failed to load logs:", e);
  }
}

// ───────────────────────── 交互 ─────────────────────────

function showSettingsModal() {
  el("settings-modal").classList.remove("hidden");
}

function hideSettingsModal() {
  el("settings-modal").classList.add("hidden");
}

function showAboutModal() {
  el("about-modal").classList.remove("hidden");
}

function hideAboutModal() {
  el("about-modal").classList.add("hidden");
}

async function saveSettings() {
  const language = el<HTMLSelectElement>("language-select").value;
  const port = Number.parseInt(el<HTMLInputElement>("port-input").value, 10);
  // 置灰的复选框在部分实现里仍会保留 checked，这里再兜一层，
  // 免得把一个后端明确不支持的开关原样提交上去。
  const autostart =
    autostartSupported && el<HTMLInputElement>("autostart-checkbox").checked;
  const webAccess = el<HTMLInputElement>("web-access-checkbox").checked;

  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    alert(t("port_invalid"));
    return;
  }

  const settings: Settings = { language, port, autostart, web_access: webAccess };

  try {
    const result = await api.saveSettings(settings);
    currentLang = language;
    updateTexts();
    hideSettingsModal();

    // 浏览器模式下改端口 / 改访问范围，服务会换到新地址，当前页面的 origin
    // 随即失效、后续所有请求都会静默失败。这里主动带过去，
    // 别让人对着一个不再刷新的页面发呆。
    if (detectMode() === "web") {
      const url = (result as { url?: string } | null | undefined)?.url;
      if (typeof url === "string" && url !== "" && url !== location.origin) {
        window.location.href = url;
      }
    }
  } catch (e) {
    console.error("Failed to save settings:", e);
    alert(String(e));
  }
}

/** 切换日志面板。浏览器模式下没有托盘菜单，这个入口是唯一的。 */
function toggleLogs() {
  const section = el("logs-section");
  const willShow = section.classList.contains("hidden");
  section.classList.toggle("hidden", !willShow);
  if (willShow) {
    void loadLogs();
  }
}

/** Tauri 模式专属：托盘菜单事件 + 后端状态推送。 */
async function setupTauriEvents() {
  await listen<Settings>("settings-changed", (event) => {
    currentLang = event.payload.language;
    updateTexts();
  });

  // 后端每 5 秒推一次完整状态，UI 因此不需要自己轮询（P2-19）
  await listen<NetworkStatus>("network-status-update", (event) => {
    currentStatus = event.payload;
    updateStatusDisplay();
    updateHistoryDisplay();
  });

  await listen("open-settings", () => {
    void loadSettings();
    showSettingsModal();
  });

  await listen("open-logs", () => {
    el("logs-section").classList.remove("hidden");
    void loadLogs();
  });

  await listen("open-about", () => {
    showAboutModal();
  });

  // 端口被占用等运行时故障必须让用户看到。注意启动阶段那次 emit 会早于本监听器
  // 注册而丢失，那一次由 init() 里的 refreshWebStatus() 主动查来兜底。
  await listen<string>("web-server-error", (event) => {
    console.error("Web server failed to start:", event.payload);
    alert(`${t("port_in_use")}\n${event.payload}`);
    void refreshWebStatus();
  });
}

function setupDomListeners() {
  el("settings-btn").addEventListener("click", () => {
    void loadSettings();
    showSettingsModal();
  });

  el("logs-btn").addEventListener("click", toggleLogs);
  el("close-settings-btn").addEventListener("click", hideSettingsModal);
  el("cancel-settings-btn").addEventListener("click", hideSettingsModal);
  el("save-settings-btn").addEventListener("click", () => void saveSettings());
  el("close-about-btn").addEventListener("click", hideAboutModal);
  el("refresh-logs-btn").addEventListener("click", () => void loadLogs());

  el("settings-modal").addEventListener("click", (e) => {
    if (e.target === el("settings-modal")) {
      hideSettingsModal();
    }
  });

  el("about-modal").addEventListener("click", (e) => {
    if (e.target === el("about-modal")) {
      hideAboutModal();
    }
  });
}

async function init() {
  setupDomListeners();

  if (detectMode() === "tauri") {
    await setupTauriEvents();
  }

  el("logs-section").classList.add("hidden");

  await loadSettings();
  await loadStatus();
  await refreshWebStatus();

  if (detectMode() === "web") {
    // 浏览器里没有事件通道，只能定时拉取
    window.setInterval(() => void loadStatus(), 5000);
  }
}

window.addEventListener("DOMContentLoaded", init);
