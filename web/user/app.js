const PAGE_SIZE = 20;
const POLL_INTERVAL = 1000;

const state = {
  page: 0,
  total: 0,
  timer: null,
  loading: false,
  authenticated: false,
};
const API_BASE = "/user/api";

const elements = {
  loginView: document.querySelector("#login-view"),
  dashboardView: document.querySelector("#dashboard-view"),
  loginForm: document.querySelector("#login-form"),
  loginButton: document.querySelector("#login-button"),
  loginError: document.querySelector("#login-error"),
  apiKey: document.querySelector("#api-key"),
  toggleKey: document.querySelector("#toggle-key"),
  logoutButton: document.querySelector("#logout-button"),
  refreshButton: document.querySelector("#refresh-button"),
  userName: document.querySelector("#user-name"),
  connectionStatus: document.querySelector("#connection-status"),
  logRows: document.querySelector("#log-rows"),
  emptyState: document.querySelector("#empty-state"),
  logCount: document.querySelector("#log-count"),
  previousPage: document.querySelector("#previous-page"),
  nextPage: document.querySelector("#next-page"),
  pageStatus: document.querySelector("#page-status"),
  metricRequests: document.querySelector("#metric-requests"),
  metricSuccess: document.querySelector("#metric-success"),
  metricErrors: document.querySelector("#metric-errors"),
  metricInput: document.querySelector("#metric-input"),
  metricOutput: document.querySelector("#metric-output"),
  limitConcurrency: document.querySelector("#limit-concurrency"),
  allowedModels: document.querySelector("#allowed-models"),
  dailyTokens: document.querySelector("#daily-tokens"),
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
  let payload = {};
  try {
    payload = await response.json();
  } catch {
    payload = { error: "服务返回了无法解析的响应" };
  }
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
  elements.apiKey.value = "";
  window.setTimeout(() => elements.apiKey.focus(), 0);
}

function showDashboard(user) {
  state.authenticated = true;
  elements.loginView.hidden = true;
  elements.dashboardView.hidden = false;
  updateUser(user);
  startPolling();
}

function updateUser(user) {
  elements.userName.textContent = user.name || "未命名 Key";
  elements.limitConcurrency.textContent = String(user.max_concurrency ?? "-");
  elements.allowedModels.textContent = formatAccessList(user.allowed_models, "全部模型");
  elements.dailyTokens.textContent = formatDailyTokens(
    user.today_tokens,
    user.daily_token_limit,
  );
  elements.allowedModels.title = elements.allowedModels.textContent;
  elements.dailyTokens.title = elements.dailyTokens.textContent;
}

function formatDailyTokens(used, limit) {
  const displayUsed = formatNumber(used);
  if (!limit || Number(limit) <= 0) return `${displayUsed} / 不限`;
  return `${displayUsed} / ${formatNumber(limit)}`;
}

function formatAccessList(values, fallback) {
  if (!Array.isArray(values) || values.length === 0 || values.includes("*")) {
    return fallback;
  }
  return values.join("、");
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
  if (milliseconds === null || milliseconds === undefined) {
    return "-";
  }
  return (Number(milliseconds) / 1000).toFixed(2);
}

function statusLabel(status) {
  if (status === "ok") return ["成功", "ok"];
  if (status === "error") return ["失败", "error"];
  if (status === "running" || status === "stream_started") return ["运行中", "running"];
  return [status || "-", ""];
}

function modelLabel(row) {
  if (!row.upstream_model || row.upstream_model === row.request_model) {
    return row.request_model || "-";
  }
  return `${row.request_model} → ${row.upstream_model}`;
}

function renderDashboard(payload) {
  const summary = payload.summary || {};
  state.total = Number(payload.total || 0);
  updateUser(payload.user || {});
  elements.metricRequests.textContent = formatNumber(summary.requests);
  elements.metricSuccess.textContent = formatNumber(summary.success);
  elements.metricErrors.textContent = formatNumber(summary.errors);
  elements.metricInput.textContent = formatNumber(summary.input_tokens);
  elements.metricOutput.textContent = formatNumber(summary.output_tokens);
  elements.dailyTokens.textContent = formatDailyTokens(
    summary.today_tokens,
    payload.user?.daily_token_limit,
  );
  elements.dailyTokens.title = elements.dailyTokens.textContent;
  elements.logCount.textContent = `共 ${formatNumber(state.total)} 条`;

  elements.logRows.replaceChildren();
  const rows = Array.isArray(payload.logs) ? payload.logs : [];
  for (const row of rows) {
    elements.logRows.append(createLogRow(row));
  }
  elements.emptyState.hidden = rows.length > 0;
  renderPagination();
}

function createLogRow(row) {
  const tr = document.createElement("tr");
  const [label, className] = statusLabel(row.status);
  appendCell(tr, row.ts || "-", row.ts || "-");

  const statusCell = document.createElement("td");
  const status = document.createElement("span");
  status.className = `status ${className}`;
  status.textContent = label;
  statusCell.append(status);
  tr.append(statusCell);

  appendCell(tr, row.api || "-");
  appendCell(tr, modelLabel(row));
  appendCell(tr, `${formatNumber(row.input_tokens)}/${formatNumber(row.output_tokens)}`);
  appendCell(tr, `${seconds(row.latency_ms)} / ${seconds(row.first_token_ms)}`);

  const errorCell = document.createElement("td");
  errorCell.textContent = row.error || "-";
  errorCell.title = row.error || "";
  if (row.error) errorCell.className = "error-text";
  tr.append(errorCell);
  return tr;
}

function appendCell(row, value, title = value) {
  const cell = document.createElement("td");
  cell.textContent = value;
  cell.title = title;
  row.append(cell);
}

function renderPagination() {
  const pages = Math.max(1, Math.ceil(state.total / PAGE_SIZE));
  if (state.page >= pages) state.page = pages - 1;
  elements.pageStatus.textContent = `第 ${state.page + 1} / ${pages} 页`;
  elements.previousPage.disabled = state.page <= 0;
  elements.nextPage.disabled = state.page + 1 >= pages;
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

elements.loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  elements.loginError.textContent = "";
  elements.loginButton.disabled = true;
  elements.loginButton.textContent = "登录中";
  try {
    const payload = await api(`${API_BASE}/login`, {
      method: "POST",
      body: JSON.stringify({ api_key: elements.apiKey.value }),
    });
    state.page = 0;
    showDashboard(payload.user);
  } catch (error) {
    elements.loginError.textContent = error.message;
  } finally {
    elements.loginButton.disabled = false;
    elements.loginButton.textContent = "登录";
  }
});

elements.toggleKey.addEventListener("click", () => {
  const visible = elements.apiKey.type === "text";
  elements.apiKey.type = visible ? "password" : "text";
  const label = visible ? "显示 API Key" : "隐藏 API Key";
  elements.toggleKey.title = label;
  elements.toggleKey.setAttribute("aria-label", label);
  elements.apiKey.focus();
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

document.addEventListener("visibilitychange", () => {
  if (!document.hidden && state.authenticated) loadDashboard();
});

async function boot() {
  try {
    const payload = await api(`${API_BASE}/session`);
    showDashboard(payload.user);
  } catch {
    showLogin();
  }
}

boot();
