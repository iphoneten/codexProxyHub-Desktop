const PAGE_SIZE = 20;
const POLL_INTERVAL = 1500;
const API_BASE = "/admin/api";

const state = {
  authenticated: false,
  loading: false,
  page: 0,
  total: 0,
  timer: null,
  view: "overview",
};

const elements = {
  loginView: document.querySelector("#login-view"),
  dashboardView: document.querySelector("#dashboard-view"),
  loginForm: document.querySelector("#login-form"),
  loginButton: document.querySelector("#login-button"),
  loginError: document.querySelector("#login-error"),
  adminKey: document.querySelector("#admin-key"),
  toggleKey: document.querySelector("#toggle-key"),
  logoutButton: document.querySelector("#logout-button"),
  refreshButton: document.querySelector("#refresh-button"),
  adminName: document.querySelector("#admin-name"),
  connectionStatus: document.querySelector("#connection-status"),
  metricRequests: document.querySelector("#metric-requests"),
  metricSuccess: document.querySelector("#metric-success"),
  metricErrors: document.querySelector("#metric-errors"),
  metricRunning: document.querySelector("#metric-running"),
  metricTokens: document.querySelector("#metric-tokens"),
  metricLatency: document.querySelector("#metric-latency"),
  providerCount: document.querySelector("#provider-count"),
  providerRows: document.querySelector("#provider-rows"),
  providerEmpty: document.querySelector("#provider-empty"),
  apiKeyCount: document.querySelector("#api-key-count"),
  apiKeyRows: document.querySelector("#api-key-rows"),
  apiKeyEmpty: document.querySelector("#api-key-empty"),
  overviewView: document.querySelector("#overview-view"),
  logsView: document.querySelector("#logs-view"),
  logCount: document.querySelector("#log-count"),
  logRows: document.querySelector("#log-rows"),
  logEmpty: document.querySelector("#log-empty"),
  previousPage: document.querySelector("#previous-page"),
  nextPage: document.querySelector("#next-page"),
  pageStatus: document.querySelector("#page-status"),
  tabs: Array.from(document.querySelectorAll(".tab-button")),
};

async function api(path, options = {}) {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: {
      "Content-Type": "application/json",
      ...(options.headers || {}),
    },
    ...options,
  });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(payload.error || `请求失败 (${response.status})`);
    error.status = response.status;
    throw error;
  }
  return payload;
}

function showLogin(message = "") {
  state.authenticated = false;
  stopPolling();
  elements.dashboardView.hidden = true;
  elements.loginView.hidden = false;
  elements.loginError.textContent = message;
  elements.adminKey.value = "";
  window.setTimeout(() => elements.adminKey.focus(), 0);
}

function showDashboard(admin) {
  state.authenticated = true;
  elements.loginView.hidden = true;
  elements.dashboardView.hidden = false;
  elements.adminName.textContent = admin?.name || "管理员";
  startPolling();
}

function formatNumber(value) {
  const number = Number(value || 0);
  if (number >= 1_000_000) {
    return `${(number / 1_000_000).toFixed(number >= 10_000_000 ? 0 : 1)}M`;
  }
  if (number >= 1_000) {
    return `${(number / 1_000).toFixed(number >= 100_000 ? 0 : 1)}K`;
  }
  return new Intl.NumberFormat("zh-CN").format(number);
}

function seconds(milliseconds) {
  if (milliseconds === null || milliseconds === undefined) return "-";
  return (Number(milliseconds) / 1000).toFixed(2);
}

function percentage(success, errors) {
  const completed = Number(success || 0) + Number(errors || 0);
  if (completed <= 0) return "-";
  return `${((Number(success || 0) / completed) * 100).toFixed(1)}%`;
}

function accessList(values) {
  if (!Array.isArray(values) || values.length === 0 || values.includes("*")) {
    return "全部模型";
  }
  return values.join("、");
}

function dailyTokenText(used, limit) {
  const displayUsed = formatNumber(used);
  if (!limit || Number(limit) <= 0) return `${displayUsed} / 不限`;
  return `${displayUsed} / ${formatNumber(limit)}`;
}

function statusLabel(status) {
  if (status === "ok" || status === "stream_started") return ["成功", "ok"];
  if (status === "running") return ["运行中", "running"];
  if (status === "raw" || status === "-") return ["原始", "disabled"];
  return ["失败", "error"];
}

function modelLabel(row) {
  if (!row.upstream_model || row.upstream_model === row.request_model) {
    return row.request_model || "-";
  }
  return `${row.request_model} → ${row.upstream_model}`;
}

function appendCell(row, value, title = value, className = "") {
  const cell = document.createElement("td");
  cell.textContent = value;
  cell.title = title;
  if (className) cell.className = className;
  row.append(cell);
}

function appendStatus(row, label, className) {
  const cell = document.createElement("td");
  const status = document.createElement("span");
  status.className = `status ${className}`;
  status.textContent = label;
  cell.append(status);
  row.append(cell);
}

function renderSummary(summary) {
  elements.metricRequests.textContent = formatNumber(summary.requests);
  elements.metricSuccess.textContent = formatNumber(summary.success);
  elements.metricErrors.textContent = formatNumber(summary.errors);
  elements.metricRunning.textContent = formatNumber(summary.running);
  elements.metricTokens.textContent = formatNumber(
    Number(summary.input_tokens || 0) + Number(summary.output_tokens || 0),
  );
  elements.metricLatency.textContent =
    Number(summary.avg_latency_ms || 0) > 0 ? `${seconds(summary.avg_latency_ms)}s` : "-";
}

function renderProviders(providers) {
  elements.providerRows.replaceChildren();
  elements.providerCount.textContent = `${providers.length} 个渠道`;
  for (const provider of providers) {
    const row = document.createElement("tr");
    appendStatus(row, provider.enabled ? "启用" : "未启用", provider.enabled ? "enabled" : "disabled");
    appendCell(row, provider.name || "-", provider.name || "-");
    appendCell(row, provider.provider_type || "-");
    appendCell(row, `${provider.priority} / ${provider.weight}`);
    appendCell(row, formatNumber(provider.model_count));
    appendCell(row, formatNumber(provider.requests));
    appendCell(row, percentage(provider.success, provider.errors));
    appendCell(
      row,
      formatNumber(Number(provider.input_tokens || 0) + Number(provider.output_tokens || 0)),
    );
    appendCell(row, provider.last_seen || "-");
    elements.providerRows.append(row);
  }
  elements.providerEmpty.hidden = providers.length > 0;
}

function renderApiKeys(apiKeys) {
  elements.apiKeyRows.replaceChildren();
  elements.apiKeyCount.textContent = `${apiKeys.length} 个 API Key`;
  for (const key of apiKeys) {
    const row = document.createElement("tr");
    appendStatus(row, key.enabled ? "启用" : "未启用", key.enabled ? "enabled" : "disabled");
    appendCell(row, key.name || "未命名", key.name || "未命名");
    appendCell(row, String(key.max_concurrency ?? "-"));
    const models = accessList(key.allowed_models);
    appendCell(row, models, models);
    appendCell(row, formatNumber(key.requests));
    appendCell(row, dailyTokenText(key.today_tokens, key.daily_token_limit));
    appendCell(row, percentage(key.success, key.errors));
    appendCell(
      row,
      formatNumber(Number(key.input_tokens || 0) + Number(key.output_tokens || 0)),
    );
    appendCell(row, key.last_seen || "-");
    appendCell(row, key.created_at || "-");
    elements.apiKeyRows.append(row);
  }
  elements.apiKeyEmpty.hidden = apiKeys.length > 0;
}

function renderLogs(logs) {
  elements.logRows.replaceChildren();
  for (const log of logs) {
    const row = document.createElement("tr");
    const [label, className] = statusLabel(log.status);
    appendCell(row, log.ts || "-");
    appendStatus(row, label, className);
    appendCell(row, log.api_key_name || "-", log.api_key_name || "-");
    appendCell(row, log.api || "-");
    appendCell(row, log.channel || "-");
    appendCell(row, modelLabel(log), modelLabel(log));
    appendCell(row, `${formatNumber(log.input_tokens)}/${formatNumber(log.output_tokens)}`);
    appendCell(row, `${seconds(log.latency_ms)} / ${seconds(log.first_token_ms)}`);
    appendCell(row, log.error || "-", log.error || "", log.error ? "error-text" : "");
    elements.logRows.append(row);
  }
  elements.logEmpty.hidden = logs.length > 0;
}

function renderPagination() {
  const pages = Math.max(1, Math.ceil(state.total / PAGE_SIZE));
  if (state.page >= pages) state.page = pages - 1;
  elements.pageStatus.textContent = `第 ${state.page + 1} / ${pages} 页`;
  elements.previousPage.disabled = state.page <= 0;
  elements.nextPage.disabled = state.page + 1 >= pages;
}

function renderDashboard(payload) {
  const providers = Array.isArray(payload.providers) ? payload.providers : [];
  const apiKeys = Array.isArray(payload.api_keys) ? payload.api_keys : [];
  const logs = Array.isArray(payload.logs) ? payload.logs : [];
  state.total = Number(payload.total || 0);
  elements.adminName.textContent = payload.admin?.name || "管理员";
  elements.logCount.textContent = `共 ${formatNumber(state.total)} 条`;
  renderSummary(payload.summary || {});
  renderProviders(providers);
  renderApiKeys(apiKeys);
  renderLogs(logs);
  renderPagination();
}

async function loadDashboard({ quiet = false } = {}) {
  if (!state.authenticated || state.loading) return;
  state.loading = true;
  if (!quiet) elements.connectionStatus.textContent = "正在刷新";
  try {
    const payload = await api(
      `${API_BASE}/dashboard?page=${state.page}&page_size=${PAGE_SIZE}`,
    );
    renderDashboard(payload);
    elements.connectionStatus.textContent = "实时连接";
    elements.connectionStatus.style.color = "";
  } catch (error) {
    if (error.status === 401) {
      showLogin(error.message);
      return;
    }
    elements.connectionStatus.textContent = "连接异常";
    elements.connectionStatus.style.color = "var(--danger)";
  } finally {
    state.loading = false;
  }
}

function startPolling() {
  stopPolling();
  loadDashboard();
  state.timer = window.setInterval(() => {
    if (!document.hidden) loadDashboard({ quiet: true });
  }, POLL_INTERVAL);
}

function stopPolling() {
  if (state.timer !== null) {
    window.clearInterval(state.timer);
    state.timer = null;
  }
}

function switchView(view) {
  state.view = view;
  elements.overviewView.hidden = view !== "overview";
  elements.logsView.hidden = view !== "logs";
  for (const tab of elements.tabs) {
    tab.classList.toggle("active", tab.dataset.view === view);
  }
}

elements.loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  elements.loginError.textContent = "";
  elements.loginButton.disabled = true;
  elements.loginButton.textContent = "登录中";
  try {
    const payload = await api(`${API_BASE}/login`, {
      method: "POST",
      body: JSON.stringify({ admin_key: elements.adminKey.value }),
    });
    state.page = 0;
    showDashboard(payload.admin);
  } catch (error) {
    elements.loginError.textContent = error.message;
  } finally {
    elements.loginButton.disabled = false;
    elements.loginButton.textContent = "登录";
  }
});

elements.toggleKey.addEventListener("click", () => {
  const visible = elements.adminKey.type === "text";
  elements.adminKey.type = visible ? "password" : "text";
  const label = visible ? "显示管理员密钥" : "隐藏管理员密钥";
  elements.toggleKey.title = label;
  elements.toggleKey.setAttribute("aria-label", label);
  elements.adminKey.focus();
});

elements.logoutButton.addEventListener("click", async () => {
  try {
    await api(`${API_BASE}/logout`, { method: "POST", body: "{}" });
  } finally {
    showLogin();
  }
});

elements.refreshButton.addEventListener("click", () => loadDashboard());

elements.previousPage.addEventListener("click", () => {
  if (state.page > 0) {
    state.page -= 1;
    loadDashboard();
  }
});

elements.nextPage.addEventListener("click", () => {
  const pages = Math.max(1, Math.ceil(state.total / PAGE_SIZE));
  if (state.page + 1 < pages) {
    state.page += 1;
    loadDashboard();
  }
});

for (const tab of elements.tabs) {
  tab.addEventListener("click", () => switchView(tab.dataset.view));
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden && state.authenticated) loadDashboard();
});

async function boot() {
  try {
    const payload = await api(`${API_BASE}/session`);
    showDashboard(payload.admin);
  } catch {
    showLogin();
  }
}

switchView("overview");
boot();
