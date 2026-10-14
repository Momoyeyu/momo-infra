// 组件页（/components）：组件表 + 状态徽标 + up/down/restart + SSE 日志 tail。
// 数据：GET /api/serve/components（profile 组件表）+
//       GET /api/serve/status（存活/就绪快照，checks[] 按组件名 merge）；
// 操作：POST /api/serve/{up|down|restart} {only:[name]?}（token+CSRF 经 api() 封装）；
// 日志：GET /api/serve/logs/{name}/stream SSE，断线带最后 cursor 重连。

import {
  api,
  badge,
  checkRow,
  clear,
  el,
  errorBox,
  pageHead,
  streamSSE,
} from "../app.js";

const POLL_MS = 5000;
const LOG_MAX_LINES = 500;

let profile = "normal";

export default async function render(root) {
  let disposed = false;
  const timers = new Set();
  const streams = new Map(); // name -> stop()
  // 日志行 buffer：status 轮询重绘时保留已收到内容
  const logBuffers = new Map(); // name -> [string]
  let busy = false;

  const head = pageHead("组件", {
    profile,
    onProfile: (p) => {
      profile = p;
      closeAllStreams();
      load();
    },
    onRefresh: () => load(),
  });
  // 全组件操作
  const ops = el(
    "span",
    {},
    ["up", "down", "restart"].map((a) =>
      el(
        "button",
        {
          class: "btn",
          style: "margin-right:.35rem",
          onclick: () => runAction(a, []),
        },
        `${a} 全部`
      )
    )
  );
  head.insertBefore(ops, head.lastElementChild);

  const resultBox = el("div", {});
  const body = el("div", {});
  root.append(head, resultBox, body);

  await load();
  const poll = setInterval(() => {
    if (!busy) load({ quiet: true });
  }, POLL_MS);
  timers.add(poll);

  return () => {
    disposed = true;
    timers.forEach(clearInterval);
    closeAllStreams();
  };

  function closeAllStreams() {
    for (const stop of streams.values()) stop();
    streams.clear();
  }

  async function load({ quiet = false } = {}) {
    if (!quiet) {
      clear(body);
      body.append(el("p", { class: "muted" }, "loading…"));
    }
    try {
      const q = `?profile=${profile}`;
      const [table, status] = await Promise.all([
        api(`/api/serve/components${q}`),
        api(`/api/serve/status${q}`),
      ]);
      if (disposed) return;
      draw(table.components ?? [], status.checks ?? []);
    } catch (e) {
      if (disposed || quiet) return;
      clear(body);
      body.append(errorBox(e));
    }
  }

  function draw(components, checks) {
    const byName = new Map(checks.map((c) => [c.id, c]));
    const rows = components.map((c) => {
      const st = byName.get(c.name);
      const marks = [
        c.external ? badge("", "external · 只探活") : null,
        c.local_endpoint_only ? badge("", "仅本地大脑档") : null,
      ];
      const stCell = st
        ? el("span", {}, [
            badge(st.status),
            " ",
            el("span", { class: "muted" }, st.detail),
            st.hint ? el("div", { class: "hint" }, st.hint) : null,
          ])
        : el("span", { class: "muted" }, "—");
      const actions = el(
        "td",
        {},
        ["up", "down", "restart"].map((a) =>
          el(
            "button",
            {
              class: "btn",
              style: "margin-right:.3rem",
              disabled: busy,
              dataset: { actionFor: c.name },
              onclick: () => runAction(a, [c.name]),
            },
            a
          )
        ),
        c.external
          ? null
          : el(
              "button",
              {
                class: "btn",
                onclick: (e) => toggleLog(c.name, e.target),
              },
              streams.has(c.name) ? "关闭日志" : "日志"
            )
      );
      const tr = el("tr", {}, [
        el("td", {}, [el("span", { class: "mono" }, c.name), " ", marks]),
        el("td", {}, stCell),
        actions,
      ]);
      // 展开的日志面板占整行
      if (streams.has(c.name)) {
        return [tr, logRow(c.name)];
      }
      return tr;
    });

    const tbl = el(
      "table",
      { class: "list" },
      el("tr", {}, [el("th", {}, "组件"), el("th", {}, "状态"), el("th", {}, "操作")]),
      rows
    );
    clear(body);
    body.append(el("section", { class: "card" }, tbl));
    // 恢复已打开的日志面板内容（load 刷新后保持 tail）
    for (const [name, buf] of logBuffers) {
      const panel = body.querySelector(`[data-log-panel="${name}"]`);
      if (panel) for (const line of buf) panel.append(logLine(line));
    }
  }

  function logRow(name) {
    const panel = el("div", {
      class: "log-panel mono",
      dataset: { logPanel: name },
    });
    return el("tr", {}, el("td", { colspan: "3" }, panel));
  }

  function logLine(text) {
    return el("div", { class: "log-line" }, text);
  }

  function toggleLog(name, btn) {
    if (streams.has(name)) {
      streams.get(name)();
      streams.delete(name);
      logBuffers.delete(name);
      load({ quiet: true });
      return;
    }
    logBuffers.set(name, []);
    const stop = streamSSE(`/api/serve/logs/${name}/stream?profile=${profile}`, {
      onEvent: (evt) => {
        if (evt.event !== "log" || !evt.data) return;
        let lines = [];
        try {
          lines = JSON.parse(evt.data).lines ?? [];
        } catch {
          return;
        }
        const buf = logBuffers.get(name) ?? [];
        buf.push(...lines);
        if (buf.length > LOG_MAX_LINES) buf.splice(0, buf.length - LOG_MAX_LINES);
        logBuffers.set(name, buf);
        const panel = body.querySelector(`[data-log-panel="${name}"]`);
        if (panel) {
          for (const l of lines) panel.append(logLine(l));
          while (panel.childElementCount > LOG_MAX_LINES)
            panel.firstChild.remove();
          panel.scrollTop = panel.scrollHeight;
        }
      },
    });
    streams.set(name, stop);
    load({ quiet: true }); // 重绘插入面板行
    btn.textContent = "关闭日志";
  }

  async function runAction(action, only) {
    busy = true;
    setButtons(true);
    clear(resultBox);
    resultBox.append(el("p", { class: "muted" }, `serve ${action} running…`));
    try {
      const report = await api(`/api/serve/${action}?profile=${profile}`, {
        method: "POST",
        body: { only },
      });
      if (disposed) return;
      clear(resultBox);
      const card = el(
        "section",
        { class: "card" },
        el("h2", {}, report.command, badge(report.summary.fail ? "fail" : "ok"))
      );
      for (const c of report.checks ?? []) card.append(checkRow(c));
      resultBox.append(card);
    } catch (e) {
      if (!disposed) {
        clear(resultBox);
        resultBox.append(errorBox(e));
      }
    } finally {
      busy = false;
      setButtons(false);
      load({ quiet: true });
    }
  }

  function setButtons(disabled) {
    for (const b of root.querySelectorAll("button.btn")) b.disabled = disabled;
  }
}
