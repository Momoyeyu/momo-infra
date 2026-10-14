// 配置页（/config，issue #56/#81）——schema 驱动渲染：
// - GET /api/config/schema 下发键元数据（type/values/sensitive/default/component），
//   GET /api/config?profile= 读 `.env.<profile>` 当前值（敏感键服务端已掩码）；
//   两处皆 config.schema.toml 单源，前端不维护键表——增删键改 schema 即生效。
// - 控件映射：enum(values 非空)→select；bool→switch；int→number（含 *_PORT 范围校验）；
//   str/url/path→text（url 校验 scheme）；未知 type→只读 + unsupported 标记；
//   sensitive→掩码控件（不回显，留空/哨兵项 = 不变更）。
// - 分组按 component（对齐 config.md §2.1–2.4），description 作 help 文本，
//   default 作 placeholder 与 reset 目标。
// - 编辑收集 diff → PUT /api/config（Bearer + X-CSRF-Token），前后端双重校验；
//   422 `error.fields` 逐字段回显；rollback 按钮 → POST /api/config/rollback；
//   成功 banner：restart_hint + 掩码 diff + /components 跳转链接。

import { api } from "../app.js";

// component 分组的展示序与标签（对齐 config.md §2 小节；schema 出现新值时自动追加在尾部）。
const COMPONENT_ORDER = ["common", "app", "models", "infra"];
const COMPONENT_LABELS = {
  common: "common — shared",
  app: "app — orchestrator",
  models: "models — model services",
  infra: "infra — momoctl/brain",
};

// ---------- 页面骨架 ----------

export default async function render(root) {
  root.innerHTML = `
    <h1>config</h1>
    <div class="toolbar">
      <label>profile
        <select id="cfg-profile">
          <option value="normal">normal</option>
          <option value="degraded">degraded</option>
        </select>
      </label>
      <span id="cfg-active" class="muted"></span>
      <button id="cfg-reload">reload</button>
      <button id="cfg-rollback">rollback .bak</button>
    </div>
    <div id="cfg-banner" class="hidden"></div>
    <form id="cfg-form"><div id="cfg-rows">loading…</div></form>`;

  const el = (id) => root.querySelector(`#${id}`);
  const profileSel = el("cfg-profile");
  // 本次渲染的键元数据（name → spec），供 reset/校验复用。
  let specByName = new Map();

  el("cfg-reload").onclick = (e) => {
    e.preventDefault();
    load();
  };
  el("cfg-rollback").onclick = (e) => {
    e.preventDefault();
    rollback();
  };
  profileSel.onchange = () => load();
  el("cfg-form").onsubmit = (e) => {
    e.preventDefault();
    save();
  };
  // 「default」按钮：把控件重置为 schema default（事件委托）。
  el("cfg-rows").onclick = (e) => {
    const b = e.target.closest(".cfg-reset");
    if (!b) return;
    e.preventDefault();
    const spec = specByName.get(b.dataset.key);
    const tr = b.closest("tr");
    const ctrl = tr && tr.querySelector("[name]");
    if (spec && ctrl) setControl(ctrl, spec.default);
  };

  async function load() {
    banner(null);
    el("cfg-rows").textContent = "loading…";
    // schema 每次重新拉取：config.schema.toml 增删键后刷新即生效（#81）。
    try {
      const [s, c] = await Promise.all([
        api("/api/config/schema"),
        api(`/api/config?profile=${profileSel.value}`),
      ]);
      el("cfg-active").textContent =
        `active: ${c.active_profile}` +
        (c.file_exists
          ? ""
          : `  (file missing — run \`momoctl switch --to ${c.profile}\` first)`);
      profileSel.value = c.profile;
      specByName = new Map(s.keys.map((k) => [k.name, k]));
      renderRows(el("cfg-rows"), s.keys, c.entries);
    } catch (e) {
      fail(el("cfg-rows"), e);
    }
  }

  // ---------- schema 驱动渲染 ----------

  function renderRows(host, keys, entries) {
    host.innerHTML = "";
    const byKey = new Map(entries.map((e) => [e.key, e]));
    const known = new Set(keys.map((k) => k.name));

    // 按 component 分组；先按固定序，再把 schema 新冒出的 component 排尾。
    const groups = new Map();
    for (const k of keys) {
      const g = k.component || "other";
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(k);
    }
    const order = [
      ...COMPONENT_ORDER.filter((c) => groups.has(c)),
      ...[...groups.keys()].filter((c) => !COMPONENT_ORDER.includes(c)),
    ];
    for (const comp of order) {
      host.appendChild(section(comp, groups.get(comp), byKey));
    }

    // schema 外键（passthrough / 游离键）：只读展示，不参与 PUT。
    const extras = entries.filter((e) => !known.has(e.key));
    if (extras.length) host.appendChild(extraSection(extras));

    const submit = document.createElement("button");
    submit.type = "submit";
    submit.textContent = "save changes";
    submit.className = "primary";
    host.appendChild(submit);
  }

  function section(comp, keys, byKey) {
    const wrap = document.createElement("section");
    wrap.className = "cfg-section";
    const h = document.createElement("h2");
    h.textContent = COMPONENT_LABELS[comp] || comp;
    wrap.appendChild(h);
    const table = document.createElement("table");
    table.className = "cfg-table";
    for (const spec of keys) {
      table.appendChild(row(spec, byKey.get(spec.name)));
    }
    wrap.appendChild(table);
    return wrap;
  }

  function row(spec, entry) {
    const tr = document.createElement("tr");
    tr.dataset.key = spec.name;
    const label = document.createElement("td");
    label.innerHTML =
      `<code title="${esc(spec.description)}">${spec.name}</code>` +
      (spec.sensitive ? ' <span class="tag">sensitive</span>' : "") +
      (isKnownType(spec) ? "" : ' <span class="tag warn">unsupported</span>') +
      (spec.default !== ""
        ? ` <button type="button" class="mini cfg-reset" data-key="${spec.name}" title="reset to default">↺</button>`
        : "") +
      `<div class="help">${esc(spec.description)}</div>`;
    const cell = document.createElement("td");
    cell.innerHTML = controlHtml(spec, entry);
    const err = document.createElement("td");
    err.className = "field-err";
    tr.append(label, cell, err);
    return tr;
  }

  function extraSection(extras) {
    const wrap = document.createElement("section");
    wrap.className = "cfg-section";
    const h = document.createElement("h2");
    h.textContent = "other — not in schema (read-only)";
    wrap.appendChild(h);
    const table = document.createElement("table");
    table.className = "cfg-table";
    for (const e of extras) {
      const tr = document.createElement("tr");
      tr.dataset.key = e.key;
      const label = document.createElement("td");
      label.innerHTML = `<code>${esc(e.key)}</code>` +
        (e.sensitive ? ' <span class="tag">sensitive</span>' : "") +
        ' <span class="tag">passthrough</span>';
      const cell = document.createElement("td");
      cell.innerHTML = `<input value="${esc(e.value ?? "")}" disabled />`;
      const err = document.createElement("td");
      tr.append(label, cell, err);
      table.appendChild(tr);
    }
    wrap.appendChild(table);
    return wrap;
  }

  // 控件 HTML。data-orig 记录渲染时原值用于 diff；敏感控件留空/哨兵 = 不变更。
  function controlHtml(spec, entry) {
    const val = entry && entry.set ? entry.value : "";
    const ph = spec.default !== "" ? `default: ${spec.default}` : "";
    if (spec.sensitive) {
      const state = entry && entry.set ? `set (${entry.value})` : "unset";
      // enum/bool 敏感项（如 MOMO_DATA_KEEP_AUDIO）：select 哨兵项「keep current」。
      const selVals = hasValues(spec)
        ? spec.values
        : spec.type === "bool"
          ? ["true", "false"]
          : null;
      if (selVals) {
        const opts = [
          `<option value="" selected>keep current (${state})</option>`,
          ...selVals.map(
            (v) => `<option value="${esc(v)}">${esc(v)}</option>`,
          ),
        ].join("");
        return `<select name="${spec.name}" data-orig="" data-sensitive="1">${opts}</select>`;
      }
      return `<input name="${spec.name}" type="password" data-orig="" data-sensitive="1" ` +
        `placeholder="${esc(state)}" autocomplete="off" />`;
    }
    if (hasValues(spec)) {
      // enum：未设置时给「unset (default: x)」哨兵——写空值服务端会 422，选哨兵=不变更。
      const opts = [
        ...(entry && entry.set
          ? []
          : [`<option value="" selected>unset (default: ${esc(spec.default)})</option>`]),
        ...spec.values.map(
          (v) =>
            `<option value="${esc(v)}"${v === val ? " selected" : ""}>${esc(v)}</option>`,
        ),
      ].join("");
      return `<select name="${spec.name}" data-orig="${esc(val)}">${opts}</select>`;
    }
    switch (spec.type) {
      case "bool": {
        const on = (entry && entry.set ? val : spec.default) === "true";
        return `<input name="${spec.name}" type="checkbox" class="cfg-switch" ` +
          `data-orig="${on}"${on ? " checked" : ""} />`;
      }
      case "int":
        return `<input name="${spec.name}" type="number" step="1" value="${esc(val)}" ` +
          `data-orig="${esc(val)}" placeholder="${esc(ph)}" />`;
      case "str":
      case "string":
      case "url":
      case "path":
        return `<input name="${spec.name}" value="${esc(val)}" ` +
          `data-orig="${esc(val)}" placeholder="${esc(ph)}" />`;
      default:
        // 未知 type：只读降级，不阻断页面（schema 演进时旧前端不崩）。
        return `<input name="${spec.name}" value="${esc(val)}" disabled ` +
          `data-orig="${esc(val)}" placeholder="unsupported type '${esc(spec.type)}'" />`;
    }
  }

  function setControl(ctrl, def) {
    if (ctrl.type === "checkbox") {
      ctrl.checked = def === "true";
    } else if (ctrl.tagName === "SELECT") {
      const ok = [...ctrl.options].some((o) => o.value === def);
      ctrl.value = ok ? def : "";
    } else {
      ctrl.value = def;
    }
  }

  // ---------- 保存 / 回滚 ----------

  async function save() {
    const changes = {};
    const clientErrs = {};
    for (const ctrl of root.querySelectorAll("#cfg-rows [name]")) {
      if (ctrl.disabled) continue;
      const spec = specByName.get(ctrl.name);
      if (!spec) continue;
      let v;
      if (ctrl.type === "checkbox") {
        v = String(ctrl.checked);
      } else {
        v = ctrl.value;
      }
      // 敏感控件留空/哨兵 = 不变更；其余与 data-orig 相同 = 不变更。
      if (v === (ctrl.dataset.orig ?? "")) continue;
      const msg = validateClient(spec, v);
      if (msg) {
        clientErrs[ctrl.name] = msg;
        continue;
      }
      changes[ctrl.name] = v;
    }
    if (Object.keys(clientErrs).length > 0) {
      showFieldErrors(clientErrs);
      return;
    }
    if (Object.keys(changes).length === 0) {
      banner({ kind: "info", text: "no changes" });
      return;
    }
    clearFieldErrors();
    try {
      const body = await api("/api/config", {
        method: "PUT",
        body: { profile: profileSel.value, changes },
      });
      banner({
        kind: "ok",
        text: `${body.restart_hint}`,
        diff: body.diff,
        link: { href: "/components", label: "go to /components" },
      });
      load();
    } catch (e) {
      if (e.status === 422 && e.fields) return showFieldErrors(e.fields);
      banner({ kind: "err", text: errText(e) });
    }
  }

  async function rollback() {
    clearFieldErrors();
    try {
      const body = await api("/api/config/rollback", {
        method: "POST",
        body: { profile: profileSel.value },
      });
      banner({
        kind: "ok",
        text: `restored from ${body.backup}. ${body.restart_hint}`,
        diff: body.diff,
      });
      load();
    } catch (e) {
      banner({ kind: "err", text: errText(e) });
    }
  }

  function showFieldErrors(fields) {
    banner({ kind: "err", text: "validation failed — see fields" });
    if (!fields) return;
    for (const [k, msg] of Object.entries(fields)) {
      const tr = root.querySelector(`tr[data-key="${CSS.escape(k)}"]`);
      const td = tr && tr.querySelector(".field-err");
      if (td) td.textContent = msg;
    }
  }

  function clearFieldErrors() {
    for (const td of root.querySelectorAll(".field-err")) td.textContent = "";
  }

  function banner(b) {
    const host = el("cfg-banner");
    if (!b) {
      host.className = "hidden";
      host.innerHTML = "";
      return;
    }
    host.className = `banner ${b.kind}`;
    const parts = [`<div>${esc(b.text)}</div>`];
    if (b.diff && b.diff.length) {
      parts.push(
        "<ul>" +
          b.diff
            .map(
              (d) =>
                `<li><code>${d.key}</code>: ${esc(d.from ?? "(unset)")} → ${esc(
                  d.to ?? "(unset)",
                )}</li>`,
            )
            .join("") +
          "</ul>",
      );
    }
    if (b.link) parts.push(`<a href="${b.link.href}">${esc(b.link.label)}</a>`);
    host.innerHTML = parts.join("");
  }

  await load();
}

// ---------- schema 辅助（与 env_file::validate_value 同口径的双重校验） ----------

function hasValues(spec) {
  return Array.isArray(spec.values) && spec.values.length > 0;
}

function isKnownType(spec) {
  return (
    hasValues(spec) ||
    ["enum", "bool", "int", "str", "string", "url", "path"].includes(spec.type)
  );
}

// 前端即时校验（服务端 PUT 仍复核；错误文案同口径便于比对）。
function validateClient(spec, v) {
  const t = spec.type;
  if (v === "") {
    return t === "str" || t === "string" || t === "path"
      ? null
      : `value must not be empty (type ${t} requires a value)`;
  }
  if (hasValues(spec)) {
    return spec.values.includes(v)
      ? null
      : `'${v}' not in allowed values ${JSON.stringify(spec.values)}`;
  }
  switch (t) {
    case "bool":
      return v === "true" || v === "false"
        ? null
        : `'${v}' is not a bool (expected true|false)`;
    case "int": {
      if (!/^-?\d+$/.test(v)) return `'${v}' is not an int`;
      if (spec.name.includes("PORT")) {
        const n = Number(v);
        if (n < 1 || n > 65535) return `'${v}' is not a valid port (1-65535)`;
      }
      return null;
    }
    case "url":
      return v.includes("://")
        ? null
        : `'${v}' is missing a URL scheme (e.g. http:// or ws://)`;
    default:
      return null; // str/string/path/未知 type：不做客户端断言，服务端为准
  }
}

function esc(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function errText(e) {
  return e?.message || "request failed";
}

function fail(host, e, prefix) {
  host.innerHTML = `<p class="err">${esc(
    (prefix ? prefix + ": " : "") + errText(e),
  )}</p>`;
}
