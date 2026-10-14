// momo console SPA 框架：前端路由 + /api fetch 封装 + fetch 版 SSE + 共享渲染件。
// 无构建链；页面按 `assets/pages/<name>.js` 懒加载（缺失时显示占位提示）。

// ---------- DOM 小工具 ----------

/** el("div", {class:"x", onclick}, ...children)；children 字符串自动 textContent。 */
export function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") node.className = v;
      else if (k.startsWith("on") && typeof v === "function")
        node.addEventListener(k.slice(2), v);
      else if (k === "dataset") Object.assign(node.dataset, v);
      else node.setAttribute(k, v);
    }
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

// ---------- /api 封装：Bearer + CSRF + 统一错误 ----------

const TOKEN_KEY = "momo.web.token";
let csrfToken = null;

export class ApiError extends Error {
  constructor(status, code, message, fields) {
    super(message || `HTTP ${status}`);
    this.status = status;
    this.code = code;
    this.fields = fields ?? null;
  }
}

export function getToken() {
  return sessionStorage.getItem(TOKEN_KEY);
}

export function setToken(t) {
  if (t) sessionStorage.setItem(TOKEN_KEY, t);
  else sessionStorage.removeItem(TOKEN_KEY);
}

function authHeaders() {
  const t = getToken();
  return t ? { Authorization: `Bearer ${t}` } : {};
}

/** 统一 JSON 请求。mutation 自动携带 GET /api/csrf 签发的 X-CSRF-Token。 */
export async function api(path, { method = "GET", body } = {}) {
  const headers = { ...authHeaders() };
  const mutating = !["GET", "HEAD", "OPTIONS"].includes(method);
  if (mutating) {
    if (!csrfToken) {
      const r = await api("/api/csrf");
      csrfToken = r.csrf_token;
    }
    headers["X-CSRF-Token"] = csrfToken;
    if (body !== undefined) headers["Content-Type"] = "application/json";
  }
  const res = await fetch(path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401) {
    showTokenModal();
    throw new ApiError(401, "unauthorized", "MOMO_WEB_TOKEN required");
  }
  const data = await res.json().catch(() => null);
  if (!res.ok) {
    throw new ApiError(
      res.status,
      data?.error?.code ?? "error",
      data?.error?.message ?? `HTTP ${res.status}`,
      data?.error?.fields
    );
  }
  return data;
}

// ---------- token modal ----------

let pendingRetry = null;

function showTokenModal() {
  const modal = document.getElementById("token-modal");
  modal.classList.remove("hidden");
  document.getElementById("token-input").focus();
}

document.getElementById("token-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const v = document.getElementById("token-input").value.trim();
  if (!v) return;
  setToken(v);
  document.getElementById("token-modal").classList.add("hidden");
  document.getElementById("token-input").value = "";
  if (pendingRetry) {
    const f = pendingRetry;
    pendingRetry = null;
    f();
  } else {
    render();
  }
});

// ---------- SSE：fetch 流式读取（EventSource 无法带 Authorization 头）----------
// onEvent({id, event, data})；断线自动带 last id 经 ?cursor= 重连（有界 backlog）。

export function streamSSE(path, { onEvent, onError } = {}) {
  const ctrl = new AbortController();
  let lastId = null;
  let stopped = false;

  async function pump() {
    while (!stopped) {
      const url = lastId ? `${path}?cursor=${lastId}` : path;
      try {
        const res = await fetch(url, {
          headers: authHeaders(),
          signal: ctrl.signal,
        });
        if (res.status === 401) {
          showTokenModal();
          stopped = true;
          return;
        }
        if (!res.ok || !res.body) {
          onError?.(new ApiError(res.status, "sse", `SSE HTTP ${res.status}`));
          stopped = true;
          return;
        }
        const reader = res.body.getReader();
        const dec = new TextDecoder();
        let buf = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          // SSE 帧以空行分隔；逐帧解析 id:/event:/data:。
          let idx;
          while ((idx = buf.indexOf("\n\n")) >= 0) {
            const frame = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            const evt = { id: null, event: "message", data: [] };
            for (const line of frame.split("\n")) {
              if (line.startsWith(":")) continue;
              const sep = line.indexOf(":");
              const field = sep < 0 ? line : line.slice(0, sep);
              let val = sep < 0 ? "" : line.slice(sep + 1);
              if (val.startsWith(" ")) val = val.slice(1);
              if (field === "id") evt.id = val;
              else if (field === "event") evt.event = val;
              else if (field === "data") evt.data.push(val);
            }
            if (evt.id) lastId = evt.id;
            onEvent?.({
              id: evt.id,
              event: evt.event,
              data: evt.data.join("\n"),
            });
          }
        }
        // 连接被服务端关闭：短暂退避后带 cursor 重连。
        if (!stopped) await new Promise((r) => setTimeout(r, 1000));
      } catch (e) {
        if (stopped || e.name === "AbortError") return;
        onError?.(e);
        await new Promise((r) => setTimeout(r, 1500));
      }
    }
  }
  pump();
  return () => {
    stopped = true;
    ctrl.abort();
  };
}

// ---------- 共享渲染件 ----------

/** 状态徽标：`badge ok|warn|fail` / 自定义 label。 */
export function badge(status, label) {
  return el("span", { class: `badge ${status}` }, label ?? status);
}

/** Check 行：status / id / detail + 可选 hint 次行 + 可选跳转链接。 */
export function checkRow(check, jump) {
  const row = el(
    "div",
    { class: "check" },
    el("span", { class: `st ${check.status}` }, check.status),
    el("span", { class: "cid mono" }, check.id),
    el("span", { class: "detail" }, [
      check.detail,
      jump
        ? [" ", el("a", { class: "jump", href: jump.href }, jump.label)]
        : null,
    ])
  );
  if (check.hint) row.append(el("span", { class: "hint" }, check.hint));
  return row;
}

/** 错误展示框。 */
export function errorBox(e) {
  const msg =
    e instanceof ApiError ? `${e.code}: ${e.message}` : String(e?.message ?? e);
  return el("div", { class: "error-box" }, msg);
}

/** 页面头：标题 + profile 切换 + 刷新按钮（回调注入）。 */
export function pageHead(title, { profile, onProfile, onRefresh } = {}) {
  const head = el("div", { class: "page-head" }, el("h1", {}, title));
  if (onProfile) {
    const seg = el(
      "span",
      { class: "seg" },
      ...["normal", "degraded"].map((p) =>
        el(
          "button",
          {
            class: p === profile ? "active" : "",
            onclick: () => onProfile(p),
          },
          p
        )
      )
    );
    head.append(seg);
  }
  head.append(el("span", { class: "spacer" }));
  if (onRefresh) head.append(el("button", { class: "btn", onclick: onRefresh }, "刷新"));
  return head;
}

// ---------- 路由 ----------

const ROUTES = new Map([
  ["/", "overview"],
  ["/components", "components"],
  ["/models", "models"],
  ["/train", "train"],
  ["/config", "config"],
]);

const app = document.getElementById("app");
let cleanup = null;

async function render() {
  const path = window.location.pathname;
  const name = ROUTES.get(path) ?? "overview";
  for (const a of document.querySelectorAll("#nav a")) {
    a.classList.toggle("active", a.dataset.route === path);
  }
  if (cleanup) {
    cleanup();
    cleanup = null;
  }
  clear(app);
  try {
    const mod = await import(`./pages/${name}.js`);
    cleanup = (await mod.default(app, ctx)) ?? null;
  } catch (e) {
    if (e instanceof ApiError || e?.status) {
      app.append(errorBox(e));
    } else {
      app.append(
        el("p", { class: "muted" }, `page '${name}' is not implemented yet`)
      );
    }
  }
}

const ctx = { navigate, api, streamSSE, el, badge, checkRow, errorBox, pageHead };

function navigate(path) {
  if (path === window.location.pathname) return;
  history.pushState(null, "", path);
  render();
}

document.getElementById("nav").addEventListener("click", (e) => {
  const a = e.target.closest("a[data-route]");
  if (!a) return;
  e.preventDefault();
  navigate(a.dataset.route);
});

window.addEventListener("popstate", render);
render();