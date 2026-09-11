import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface Settings {
  language: string;
  port: number;
  autostart: boolean;
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

const i18n: Record<string, Record<string, string>> = {
  zh: {
    app_title: "网络监控",
    network_status: "网络状态",
    online: "在线",
    offline: "离线",
    checking: "检测中...",
    settings: "设置",
    language: "语言",
    port: "端口",
    autostart: "开机启动",
    save: "保存",
    cancel: "取消",
    disconnect_warning: "网络已断开！",
    disconnect_duration: "断网持续时间",
    disconnect_count: "断网次数",
    disconnect_history: "断网记录",
    still_offline: "仍处于离线状态",
    restored: "网络已恢复",
    adapter_normal: "适配器正常",
    adapter_error: "适配器异常",
    ping_normal: "Ping正常",
    ping_failed: "Ping失败",
    view_logs: "查看日志",
    clear_logs: "清空日志",
    about: "关于",
    version: "版本",
    connection_info: "连接信息",
    settings_saved: "设置已保存",
    seconds: "秒",
    logs: "日志",
    no_logs: "暂无日志",
    no_disconnect_history: "暂无断网记录",
    last_check: "最后检查",
    adapter: "适配器",
    ping: "Ping",
    statistics: "统计",
    current_duration: "当前持续时间",
    language_changed: "语言已切换",
  },
  en: {
    app_title: "Network Monitor",
    network_status: "Network Status",
    online: "Online",
    offline: "Offline",
    checking: "Checking...",
    settings: "Settings",
    language: "Language",
    port: "Port",
    autostart: "Autostart",
    save: "Save",
    cancel: "Cancel",
    disconnect_warning: "Network Disconnected!",
    disconnect_duration: "Disconnect Duration",
    disconnect_count: "Disconnect Count",
    disconnect_history: "Disconnect History",
    still_offline: "Still offline",
    restored: "Network Restored",
    adapter_normal: "Normal",
    adapter_error: "Error",
    ping_normal: "OK",
    ping_failed: "Failed",
    view_logs: "View Logs",
    clear_logs: "Clear Logs",
    about: "About",
    version: "Version",
    connection_info: "Connection Info",
    settings_saved: "Settings Saved",
    seconds: "seconds",
    logs: "Logs",
    no_logs: "No logs",
    no_disconnect_history: "No disconnect records",
    last_check: "Last Check",
    adapter: "Adapter",
    ping: "Ping",
    statistics: "Statistics",
    current_duration: "Current Duration",
    language_changed: "Language Changed",
  },
};

let currentLang = "zh";
let currentStatus: NetworkStatus | null = null;

function t(key: string): string {
  return i18n[currentLang]?.[key] || i18n["en"]?.[key] || key;
}

function updateTexts() {
  document.getElementById("app-title")!.textContent = t("app_title");
  document.getElementById("status-title")!.textContent = t("network_status");
  document.getElementById("stats-title")!.textContent = t("statistics");
  document.getElementById("history-title")!.textContent = t("disconnect_history");
  document.getElementById("logs-title")!.textContent = t("logs");
  document.getElementById("settings-modal-title")!.textContent = t("settings");
  document.getElementById("about-modal-title")!.textContent = t("about");
  document.getElementById("language-label")!.textContent = t("language");
  document.getElementById("port-label")!.textContent = t("port");
  document.getElementById("autostart-label")!.textContent = t("autostart");
  document.getElementById("save-settings-btn")!.textContent = t("save");
  document.getElementById("cancel-settings-btn")!.textContent = t("cancel");
  document.getElementById("disconnect-count-label")!.textContent = t("disconnect_count");
  document.getElementById("current-duration-label")!.textContent = t("current_duration");
  document.getElementById("last-check-label")!.textContent = t("last_check");
  document.getElementById("adapter-label")!.textContent = t("adapter");
  document.getElementById("ping-label")!.textContent = t("ping");
  document.getElementById("about-version")!.textContent = `${t("version")} 0.1.0`;
  document.getElementById("about-desc")!.textContent = "Network monitoring tool with web interface";

  const noHistory = document.getElementById("no-history");
  if (noHistory) {
    noHistory.textContent = t("no_disconnect_history");
  }

  updateStatusDisplay();
  updateHistoryDisplay();
}

function updateStatusDisplay() {
  if (!currentStatus) return;

  const statusDot = document.getElementById("status-dot")!;
  const statusText = document.getElementById("status-text")!;
  const adapterStatus = document.getElementById("adapter-status")!;
  const pingStatus = document.getElementById("ping-status")!;
  const lastCheckTime = document.getElementById("last-check-time")!;
  const disconnectCount = document.getElementById("disconnect-count")!;
  const currentDuration = document.getElementById("current-duration")!;
  const warningSection = document.getElementById("warning-section")!;

  if (currentStatus.is_online) {
    statusDot.className = "dot online";
    statusText.textContent = t("online");
    warningSection.classList.add("hidden");
  } else {
    statusDot.className = "dot offline";
    statusText.textContent = t("offline");
    warningSection.classList.remove("hidden");

    const warningTitle = document.getElementById("warning-title")!;
    const warningDuration = document.getElementById("warning-duration")!;
    warningTitle.textContent = t("disconnect_warning");

    if (currentStatus.current_disconnect_duration) {
      warningDuration.textContent = `${t("disconnect_duration")}: ${currentStatus.current_disconnect_duration}${t("seconds")}`;
    }
  }

  adapterStatus.textContent = currentStatus.adapter_status ? t("adapter_normal") : t("adapter_error");
  adapterStatus.style.color = currentStatus.adapter_status ? "#22c55e" : "#ef4444";

  pingStatus.textContent = currentStatus.ping_success ? t("ping_normal") : t("ping_failed");
  pingStatus.style.color = currentStatus.ping_success ? "#22c55e" : "#ef4444";

  lastCheckTime.textContent = currentStatus.last_check;
  disconnectCount.textContent = currentStatus.disconnect_count.toString();

  if (currentStatus.current_disconnect_duration) {
    currentDuration.textContent = `${currentStatus.current_disconnect_duration}${t("seconds")}`;
  } else {
    currentDuration.textContent = `-${t("seconds")}`;
  }
}

function updateHistoryDisplay() {
  if (!currentStatus) return;

  const historyList = document.getElementById("history-list")!;
  const noHistory = document.getElementById("no-history")!;

  if (currentStatus.disconnect_history.length === 0) {
    noHistory.classList.remove("hidden");
    historyList.querySelectorAll(".history-item").forEach(el => el.remove());
    return;
  }

  noHistory.classList.add("hidden");

  const existingItems = historyList.querySelectorAll(".history-item");
  existingItems.forEach(el => el.remove());

  const records = [...currentStatus.disconnect_history].reverse().slice(0, 20);

  records.forEach(record => {
    const item = document.createElement("div");
    item.className = "history-item";

    const timeSpan = document.createElement("span");
    timeSpan.className = "time";
    timeSpan.textContent = record.start;

    const durationSpan = document.createElement("span");
    durationSpan.className = "duration";
    if (record.end && record.duration_secs) {
      durationSpan.textContent = `${record.duration_secs}${t("seconds")}`;
    } else {
      durationSpan.textContent = t("still_offline");
    }

    item.appendChild(timeSpan);
    item.appendChild(durationSpan);
    historyList.appendChild(item);
  });
}

async function loadSettings() {
  try {
    const settings: Settings = await invoke("get_settings");
    currentLang = settings.language;

    (document.getElementById("language-select") as HTMLSelectElement).value = settings.language;
    (document.getElementById("port-input") as HTMLInputElement).value = settings.port.toString();
    (document.getElementById("autostart-checkbox") as HTMLInputElement).checked = settings.autostart;

    updateTexts();
  } catch (e) {
    console.error("Failed to load settings:", e);
  }
}

async function loadStatus() {
  try {
    currentStatus = await invoke("get_status");
    updateStatusDisplay();
    updateHistoryDisplay();
  } catch (e) {
    console.error("Failed to load status:", e);
  }
}

async function loadLogs() {
  try {
    const logsContainer = document.getElementById("logs-container")!;
    const logs: LogEntry[] = await invoke("get_translated_logs");

    if (logs.length === 0) {
      logsContainer.innerHTML = `<div class="log-entry"><span class="timestamp">${t("no_logs")}</span></div>`;
      return;
    }

    logsContainer.innerHTML = logs.slice(0, 100).map(log => `
      <div class="log-entry">
        <span class="timestamp">${log.timestamp}</span>
        <span class="level ${log.level}">${log.level}</span>
        <span class="message">${log.translated}</span>
      </div>
    `).join("");
  } catch (e) {
    console.error("Failed to load logs:", e);
  }
}

function showSettingsModal() {
  document.getElementById("settings-modal")!.classList.remove("hidden");
}

function hideSettingsModal() {
  document.getElementById("settings-modal")!.classList.add("hidden");
}

function showAboutModal() {
  document.getElementById("about-modal")!.classList.remove("hidden");
}

function hideAboutModal() {
  document.getElementById("about-modal")!.classList.add("hidden");
}

async function saveSettings() {
  const language = (document.getElementById("language-select") as HTMLSelectElement).value;
  const port = parseInt((document.getElementById("port-input") as HTMLInputElement).value);
  const autostart = (document.getElementById("autostart-checkbox") as HTMLInputElement).checked;

  const settings: Settings = { language, port, autostart };

  try {
    await invoke("save_settings", { settings });
    currentLang = language;
    updateTexts();
    hideSettingsModal();
  } catch (e) {
    console.error("Failed to save settings:", e);
    alert(String(e));
  }
}

async function setupEventListeners() {
  document.getElementById("settings-btn")!.addEventListener("click", () => {
    loadSettings();
    showSettingsModal();
  });

  document.getElementById("close-settings-btn")!.addEventListener("click", hideSettingsModal);
  document.getElementById("cancel-settings-btn")!.addEventListener("click", hideSettingsModal);
  document.getElementById("save-settings-btn")!.addEventListener("click", saveSettings);

  document.getElementById("close-about-btn")!.addEventListener("click", hideAboutModal);

  document.getElementById("settings-modal")!.addEventListener("click", (e) => {
    if (e.target === document.getElementById("settings-modal")) {
      hideSettingsModal();
    }
  });

  document.getElementById("about-modal")!.addEventListener("click", (e) => {
    if (e.target === document.getElementById("about-modal")) {
      hideAboutModal();
    }
  });

  document.getElementById("refresh-logs-btn")!.addEventListener("click", loadLogs);

  await listen("settings-changed", async (event) => {
    const settings = event.payload as Settings;
    currentLang = settings.language;
    updateTexts();
  });

  await listen<NetworkStatus>("network-offline", (event) => {
    currentStatus = event.payload;
    updateStatusDisplay();
    updateHistoryDisplay();
  });

  await listen<NetworkStatus>("network-online", (event) => {
    currentStatus = event.payload;
    updateStatusDisplay();
    updateHistoryDisplay();
  });

  await listen<NetworkStatus>("network-status-update", (event) => {
    currentStatus = event.payload;
    updateStatusDisplay();
  });

  await listen("open-settings", () => {
    loadSettings();
    showSettingsModal();
  });

  await listen("open-logs", () => {
    const logsSection = document.getElementById("logs-section")!;
    logsSection.classList.remove("hidden");
    loadLogs();
  });

  await listen("open-about", () => {
    showAboutModal();
  });

  await listen<string>("language-changed", (event) => {
    currentLang = event.payload;
    updateTexts();
  });
}

async function init() {
  await setupEventListeners();
  await loadSettings();
  await loadStatus();

  setInterval(loadStatus, 5000);

  document.getElementById("logs-section")!.classList.add("hidden");
}

window.addEventListener("DOMContentLoaded", init);
