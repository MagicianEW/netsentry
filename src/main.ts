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

/**
 * 语言资源直接复用仓库根目录的 `i18n.json`。
 *
 * 这份文件同时被 Rust 端 `include_str!` 读取，所以前端和后端**不可能**再出现
 * 字典不一致的情况（原来两边各写一份、已经漂移了）。
 */
const i18n = dict as unknown as Record<string, Record<string, string>>;

let currentLang = "zh";
let currentStatus: NetworkStatus | null = null;

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
  el("web-access-label").textContent = t("web_access");
  el("web-access-hint").textContent = t("web_access_hint");
  el("save-settings-btn").textContent = t("save");
  el("cancel-settings-btn").textContent = t("cancel");
  el("disconnect-count-label").textContent = t("disconnect_count");
  el("current-duration-label").textContent = t("current_duration");
  el("last-check-label").textContent = t("last_check");
  el("adapter-label").textContent = t("adapter");
  el("ping-label").textContent = t("ping");
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

    el<HTMLSelectElement>("language-select").value = settings.language;
    el<HTMLInputElement>("port-input").value = settings.port.toString();
    el<HTMLInputElement>("autostart-checkbox").checked = settings.autostart;
    el<HTMLInputElement>("web-access-checkbox").checked = settings.web_access;

    updateTexts();
  } catch (e) {
    console.error("Failed to load settings:", e);
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
  const autostart = el<HTMLInputElement>("autostart-checkbox").checked;
  const webAccess = el<HTMLInputElement>("web-access-checkbox").checked;

  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    alert(t("port_invalid"));
    return;
  }

  const settings: Settings = { language, port, autostart, web_access: webAccess };

  try {
    await api.saveSettings(settings);
    currentLang = language;
    updateTexts();
    hideSettingsModal();
  } catch (e) {
    console.error("Failed to save settings:", e);
    alert(String(e));
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
    loadSettings();
    showSettingsModal();
  });

  await listen("open-logs", () => {
    el("logs-section").classList.remove("hidden");
    loadLogs();
  });

  await listen("open-about", () => {
    showAboutModal();
  });

  // 端口被占用等情况下后端启动 Web 服务失败，必须让用户看到，不能静默
  await listen<string>("web-server-error", (event) => {
    console.error("Web server failed to start:", event.payload);
    alert(`${t("port_in_use")}\n${event.payload}`);
  });
}

function setupDomListeners() {
  el("settings-btn").addEventListener("click", () => {
    loadSettings();
    showSettingsModal();
  });

  el("close-settings-btn").addEventListener("click", hideSettingsModal);
  el("cancel-settings-btn").addEventListener("click", hideSettingsModal);
  el("save-settings-btn").addEventListener("click", saveSettings);
  el("close-about-btn").addEventListener("click", hideAboutModal);
  el("refresh-logs-btn").addEventListener("click", loadLogs);

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

  if (detectMode() === "web") {
    // 浏览器里没有事件通道，只能定时拉取
    window.setInterval(loadStatus, 5000);
  }
}

window.addEventListener("DOMContentLoaded", init);
