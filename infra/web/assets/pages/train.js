// 训练页（/train，issue #42），四块联动：
// - pipeline wizard：GET /api/pipelines → 选定义 → 选执行器（exec_local /
//   exec_ssh 目标）→ 填 [vars]（required 必填、default 作 placeholder）+
//   自由 --set 行（未声明的超参）→ 预览展开命令 → POST /api/jobs
//   （{kind:"train", pipeline, set, executor, dry_run}）；
// - job 列表：GET /api/jobs 轮询（id/pipeline/status/elapsed/desc），行展开即详情；
// - job 详情：GET /api/jobs/{id}/logs SSE tail（帧 id=字节 offset，断线
//   ?cursor= 续传，同 serve logs 约定）+ GET /api/jobs/{id}/metrics 的
//   train-manifest.json → loss 曲线（内联 SVG sparkline，无外部依赖）；
// - promote：succeeded job → confirm → POST /api/jobs/{id}/promote
//   （frozen gate 通过 + 显式动作 → 原子 activate，.bak 保留回滚）。
// exec_ssh 目标选中提交会收到后端 422 字段级解释（pipeline 只走 local）。

import {
  api,
  badge,
  clear,
  el,
  errorBox,
  pageHead,
  streamSSE,
} from "../app.js";

const POLL_MS = 5000;
const LOG_MAX_LINES = 500;

// 执行器选项：pipeline 快捷提交只能走 local（pipeline step 自带 ssh 目标——
// ssh 选项保留在列表里说明这点，选中提交会收到后端 422 字段级解释）。
const EXECUTORS = [
  { value: "local", label: "exec_local — this host" },
  { value: "ssh:bigstrong", label: "exec_ssh — bigstrong (4090D) [pipelines run local]" },
  { value: "ssh:mac-studio", label: "exec_ssh — mac-studio [pipelines run local]" },
];

// job status → badge 配色
const STATUS_BADGE = {
  queued: "",
  preparing: "",
  running: "warn",
  canceling: "warn",
  recovering: "warn",
  succeeded: "ok",
  promoted: "ok",
  failed: "fail",
  canceled: "",
};

export default async function render(root) {
  let disposed = false;
  let busy = false;
  const timers = new Set();
  const open = new Set(); // 展开的 job id
  const streams = new Map(); // id -> stop()
  const logBuffers = new Map(); // id -> [string]
  const metricsCache = new Map(); // id -> metrics report
  let jobs = [];
  let pipelines = [];

  const head = pageHead("训练", { onRefresh: () => loadJobs() });
  const resultBox = el("div", {});
  const wizardBox = el("section", { class: "card" });
  const jobsBox = el("section", { class: "card" });
  root.append(head, resultBox, wizardBox, jobsBox);

  await loadPipelines();
  await loadJobs();
  const poll = setInterval(() => {
    if (!busy) loadJobs({ quiet: true });
  }, POLL_MS);
  timers.add(poll);

  return () => {
    disposed = true;
    timers.forEach(clearInterval);
    for (const stop of streams.values()) stop();
    streams.clear();
  };

  // ---------- wizard ----------

  async function loadPipelines() {
    clear(wizardBox);
    wizardBox.append(el("h2", {}, "pipeline wizard"), el("p", { class: "muted" }, "loading…"));
    try {
      const report = await api("/api/pipelines");
      if (disposed) return;
      pipelines = report.pipelines ?? [];
      drawWizard();
    } catch (e) {
      if (disposed) return;
      clear(wizardBox);
      wizardBox.append(el("h2", {}, "pipeline wizard"), errorBox(e));
    }
  }

  function drawWizard() {
    clear(wizardBox);
    wizardBox.append(el("h2", {}, "pipeline wizard"));
    if (!pipelines.length) {
      wizardBox.append(
        el("p", { class: "muted" }, "no pipeline definitions found (infra/pipelines/)")
      );
      return;
    }

    const pipeSel = el(
      "select",
      { id: "tw-pipeline", onchange: drawVars },
      pipelines.map((p) => el("option", { value: p.name }, p.name))
    );
    const execSel = el(
      "select",
      { id: "tw-executor" },
      EXECUTORS.map((x) => el("option", { value: x.value }, x.label))
    );
    const dryRun = el("input", { type: "checkbox", id: "tw-dryrun" });
    const extraVars = el("input", {
      id: "tw-extra",
      placeholder: "extra --set, e.g. lr=1e-4 steps=800",
      style: "width:100%",
    });
    const varsBox = el("div", {});
    const preview = el("p", { class: "mono muted", style: "overflow-wrap:anywhere" });

    const submitBtn = el(
      "button",
      {
        class: "btn primary",
        disabled: busy,
        onclick: () => submitWizard(),
      },
      "submit job"
    );

    wizardBox.append(
      el("table", { class: "cfg-table" }, [
        el("tr", {}, [
          el("td", {}, "pipeline"),
          el("td", {}, pipeSel),
          el("td", { class: "field-err", dataset: { ferr: "pipeline" } }),
        ]),
        el("tr", {}, [
          el("td", {}, "executor"),
          el("td", {}, [
            execSel,
            el("div", { class: "help" },
              "exec_local runs `momoctl pipeline run` here; pipeline jobs always run local (steps manage their own ssh targets)"),
          ]),
          el("td", { class: "field-err", dataset: { ferr: "executor" } }),
        ]),
        el("tr", {}, [
          el("td", {}, "dry-run"),
          el("td", {}, [
            dryRun,
            el("span", { class: "help" }, " expand plan only; no commands run, nothing registered"),
          ]),
          el("td", {}),
        ]),
      ]),
      varsBox,
      el("table", { class: "cfg-table" }, el("tr", {}, [
        el("td", {}, "extra vars"),
        el("td", {}, [extraVars, el("div", { class: "help" }, "space-separated KEY=VALUE; undeclared keys pass through like `--set`")]),
        el("td", { class: "field-err", dataset: { ferr: "set" } }),
      ])),
      preview,
      submitBtn
    );

    for (const node of [pipeSel, execSel, dryRun, extraVars]) {
      node.addEventListener("input", updatePreview);
      node.addEventListener("change", updatePreview);
    }
    drawVars();

    function curDef() {
      return pipelines.find((p) => p.name === pipeSel.value) ?? pipelines[0];
    }

    function drawVars() {
      const def = curDef();
      clear(varsBox);
      if (!def) return;
      const rows = (def.vars ?? []).map((v) =>
        el("tr", {}, [
          el("td", {}, [
            el("code", {}, v.name),
            " ",
            v.required ? badge("warn", "required") : null,
            el("div", { class: "help" }, v.description || ""),
          ]),
          el("td", {}, el("input", {
            dataset: { varName: v.name },
            placeholder: v.default ?? "",
          })),
          el("td", { class: "field-err", dataset: { ferr: `set.${v.name}` } }),
        ])
      );
      varsBox.append(
        el("p", { class: "muted", style: "margin:.4rem 0" }, [
          `${def.description || def.name}`,
          el("br"),
          `steps: ${(def.steps ?? []).join(" → ")}  ·  artifact: ${def.artifact_dir}`,
          def.activate_env ? `  ·  promote → ${def.activate_env}` : "",
        ]),
        rows.length ? el("table", { class: "cfg-table" }, rows) : null
      );
      for (const inp of varsBox.querySelectorAll("input")) {
        inp.addEventListener("input", updatePreview);
      }
      updatePreview();
    }

    function collectSet() {
      const set = {};
      for (const inp of varsBox.querySelectorAll("input[data-var-name]")) {
        const v = inp.value.trim();
        if (v !== "") set[inp.dataset.varName] = v;
      }
      for (const tok of (extraVars.value || "").split(/\s+/)) {
        if (!tok) continue;
        const i = tok.indexOf("=");
        if (i > 0) set[tok.slice(0, i)] = tok.slice(i + 1);
      }
      return set;
    }

    function previewLine() {
      const def = curDef();
      if (!def) return "";
      const parts = ["momoctl", "pipeline", "run", "--name", def.name];
      if (dryRun.checked) parts.push("--dry-run");
      for (const [k, v] of Object.entries(collectSet())) parts.push("--set", `${k}=${v}`);
      return "$ " + parts.join(" ");
    }

    function updatePreview() {
      preview.textContent = previewLine();
    }

    async function submitWizard() {
      const def = curDef();
      if (!def) return;
      clearFieldErrs();
      clear(resultBox);
      busy = true;
      submitBtn.disabled = true;
      try {
        const report = await api("/api/jobs", {
          method: "POST",
          body: {
            kind: "train",
            pipeline: def.name,
            set: collectSet(),
            ...(execSel.value.startsWith("ssh:")
              ? { executor: "ssh", ssh: { host: execSel.value.slice(4) } }
              : { executor: execSel.value }),
            dry_run: dryRun.checked,
            desc: previewLine().slice(2),
          },
        });
        if (disposed) return;
        const job = report.job;
        banner(`submitted job ${job.id} (${job.status})`, "ok");
        open.add(job.id);
        await loadJobs({ quiet: true });
        startLog(job.id);
      } catch (e) {
        if (disposed) return;
        if (e.status === 422 && e.fields) {
          showFieldErrs(e.fields);
        } else {
          clear(resultBox);
          resultBox.append(errorBox(e));
        }
      } finally {
        busy = false;
        submitBtn.disabled = false;
      }
    }

    function showFieldErrs(fields) {
      banner("validation failed — see fields", "err");
      for (const [k, msg] of Object.entries(fields)) {
        const td = wizardBox.querySelector(`[data-ferr="${CSS.escape(k)}"]`);
        if (td) td.textContent = msg;
      }
    }

    function clearFieldErrs() {
      for (const td of wizardBox.querySelectorAll("[data-ferr]")) td.textContent = "";
    }
  }

  // ---------- job 列表 + 详情 ----------

  async function loadJobs({ quiet = false } = {}) {
    try {
      const report = await api("/api/jobs");
      if (disposed) return;
      jobs = report.jobs ?? [];
      // 展开中的 job 顺带刷新 metrics（详情面板的曲线/manifest）。
      for (const id of open) {
        try {
          metricsCache.set(id, await api(`/api/jobs/${id}/metrics`));
        } catch {
          /* metrics 缺失不算错误 */
        }
      }
      if (disposed) return;
      drawJobs();
    } catch (e) {
      if (disposed || quiet) return;
      clear(jobsBox);
      jobsBox.append(el("h2", {}, "jobs"), errorBox(e));
    }
  }

  function drawJobs() {
    clear(jobsBox);
    jobsBox.append(
      el("h2", {}, "jobs", el("span", {
        class: "muted",
        style: "font-weight:400;font-size:.8rem",
      }, `${jobs.length} total`))
    );
    if (!jobs.length) {
      jobsBox.append(el("p", { class: "muted" }, "(no jobs yet — use the wizard above)"));
      return;
    }
    const rows = jobs.map((j) => {
      const tr = el("tr", {}, [
        el("td", { class: "mono", style: "white-space:nowrap" }, j.id),
        el("td", {}, pipelineOf(j) ?? el("span", { class: "muted" }, "—")),
        el("td", {}, badge(STATUS_BADGE[j.status] ?? "", j.status)),
        el("td", { class: "muted", style: "white-space:nowrap" }, fmtAge(j.created_at)),
        el("td", { class: "muted", style: "overflow-wrap:anywhere" }, j.desc || j.args?.join(" ") || ""),
        el("td", { style: "white-space:nowrap" }, [
          el("button", {
            class: "btn",
            style: "margin-right:.3rem",
            onclick: () => toggleDetail(j.id),
          }, open.has(j.id) ? "收起" : "详情"),
          !isTerminal(j.status)
            ? el("button", {
                class: "btn danger",
                style: "margin-right:.3rem",
                disabled: busy,
                onclick: () => cancelJob(j.id),
              }, "cancel")
            : null,
          j.status === "succeeded"
            ? el("button", {
                class: "btn primary",
                disabled: busy,
                onclick: () => promoteJob(j.id),
              }, "promote")
            : null,
        ]),
      ]);
      return open.has(j.id) ? [tr, detailRow(j)] : tr;
    });
    jobsBox.append(
      el("table", { class: "list" }, [
        el("tr", {}, [
          el("th", {}, "id"),
          el("th", {}, "pipeline"),
          el("th", {}, "status"),
          el("th", {}, "age"),
          el("th", {}, "desc"),
          el("th", {}, ""),
        ]),
        rows,
      ])
    );
    // 恢复日志缓冲（轮询重绘后保留 tail）
    for (const [id, buf] of logBuffers) {
      const panel = jobsBox.querySelector(`[data-log-panel="${id}"]`);
      if (panel) {
        for (const line of buf) panel.append(logLine(line));
        panel.scrollTop = panel.scrollHeight;
      }
    }
  }

  function detailRow(j) {
    const cells = el("td", { colspan: "6" });
    // record 摘要
    cells.append(
      el("div", { class: "muted", style: "font-size:.8rem;margin-bottom:.5rem" }, [
        el("div", {}, `args: ${(j.args ?? []).join(" ")}`),
        el("div", {}, `log: ${j.log_path}`),
        j.exit !== null && j.exit !== undefined ? el("div", {}, `exit: ${j.exit}`) : null,
        j.error ? el("div", { class: "err" }, `error: ${j.error}`) : null,
      ])
    );
    // metrics / loss 曲线
    const m = metricsCache.get(j.id);
    cells.append(metricsCard(m));
    // SSE 日志面板
    cells.append(
      el("div", { class: "log-panel mono", dataset: { logPanel: j.id } })
    );
    return el("tr", {}, cells);
  }

  function metricsCard(m) {
    if (!m) return el("p", { class: "muted" }, "metrics loading…");
    const box = el("div", { style: "margin:.5rem 0" });
    if (m.run_id) {
      box.append(el("div", { class: "muted", style: "font-size:.8rem" },
        `run: ${m.run_id}  ·  artifact: ${m.artifact_dir ?? "—"}`));
    }
    const manifest = m.manifest;
    if (manifest && typeof manifest === "object") {
      const series = lossSeries(manifest);
      if (series.length > 1) {
        box.append(sparkline(series));
      } else if (typeof manifest.loss === "number") {
        box.append(el("div", {}, `final loss: ${manifest.loss}`));
      }
      const summary = ["base_revision", "data_count", "lr", "steps"]
        .filter((k) => manifest[k] !== undefined)
        .map((k) => `${k}=${fmtVal(manifest[k])}`)
        .join("  ·  ");
      if (summary) box.append(el("div", { class: "muted", style: "font-size:.8rem" }, summary));
    } else {
      box.append(el("p", { class: "muted", style: "font-size:.8rem" },
        "no train-manifest.json / step metrics yet"));
    }
    const withMetrics = (m.steps ?? []).filter(
      (s) => s.metrics && Object.keys(s.metrics).length
    );
    if (withMetrics.length) {
      box.append(
        el("div", { class: "muted", style: "font-size:.78rem;margin-top:.3rem" },
          withMetrics.map((s) => `${s.name} (${s.status}): ${JSON.stringify(s.metrics)}`))
      );
    }
    return box;
  }

  async function toggleDetail(id) {
    if (open.has(id)) {
      open.delete(id);
      streams.get(id)?.();
      streams.delete(id);
      logBuffers.delete(id);
      metricsCache.delete(id);
      drawJobs();
      return;
    }
    open.add(id);
    try {
      metricsCache.set(id, await api(`/api/jobs/${id}/metrics`));
    } catch {
      /* 无 metrics 属正常 */
    }
    if (disposed) return;
    drawJobs();
    startLog(id);
  }

  function startLog(id) {
    if (streams.has(id)) return;
    logBuffers.set(id, logBuffers.get(id) ?? []);
    const stop = streamSSE(`/api/jobs/${id}/logs`, {
      onEvent: (evt) => {
        const panel = jobsBox.querySelector(`[data-log-panel="${id}"]`);
        if (evt.event === "log" && evt.data) {
          let payload;
          try {
            payload = JSON.parse(evt.data);
          } catch {
            return;
          }
          const lines = (payload.chunk ?? "").split("\n").filter((l) => l !== "");
          const buf = logBuffers.get(id) ?? [];
          buf.push(...lines);
          if (buf.length > LOG_MAX_LINES) buf.splice(0, buf.length - LOG_MAX_LINES);
          logBuffers.set(id, buf);
          if (panel) {
            for (const l of lines) panel.append(logLine(l));
            while (panel.childElementCount > LOG_MAX_LINES) panel.firstChild.remove();
            panel.scrollTop = panel.scrollHeight;
          }
        } else if (evt.event === "end" && panel) {
          let st = "";
          try {
            st = JSON.parse(evt.data).status ?? "";
          } catch { /* ignore */ }
          panel.append(logLine(`[stream closed] final status: ${st}`));
        }
      },
    });
    streams.set(id, stop);
  }

  async function cancelJob(id) {
    busy = true;
    try {
      const report = await api(`/api/jobs/${id}/cancel`, { method: "POST" });
      if (disposed) return;
      banner(`job ${id}: ${report.job.status}${report.note ? ` — ${report.note}` : ""}`, "ok");
    } catch (e) {
      if (!disposed) banner(errText(e), "err");
    } finally {
      busy = false;
      loadJobs({ quiet: true });
    }
  }

  async function promoteJob(id) {
    const m = metricsCache.get(id);
    const target = m?.artifact_dir ?? "(unresolved artifact)";
    if (!window.confirm(`promote job ${id}?\nartifact: ${target}\n(atomic env activate; .bak kept for rollback)`)) {
      return;
    }
    busy = true;
    try {
      const report = await api(`/api/jobs/${id}/promote`, { method: "POST", body: {} });
      if (disposed) return;
      banner(`job ${id}: ${report.job.status}${report.note ? ` — ${report.note}` : ""}`, "ok");
    } catch (e) {
      if (!disposed) banner(errText(e), "err");
    } finally {
      busy = false;
      loadJobs({ quiet: true });
    }
  }

  // ---------- 小件 ----------

  function banner(text, kind) {
    clear(resultBox);
    resultBox.append(el("div", { class: `banner ${kind}` }, text));
  }

  function logLine(text) {
    return el("div", { class: "log-line" }, text);
  }
}

// ---------- 纯函数 ----------

function isTerminal(status) {
  return ["succeeded", "failed", "canceled", "promoted"].includes(status);
}

/// args/desc 推导 pipeline 名（`pipeline run --name X`）。
function pipelineOf(job) {
  const text = [...(job.args ?? []), job.desc ?? ""].join(" ");
  const m = text.match(/--name[=\s]+([^\s]+)/);
  return m && text.includes("pipeline") ? m[1] : null;
}

/// epoch 毫秒 → 紧凑时长（`12s`/`3m`/`2h`/`1d`）。
function fmtAge(createdMs) {
  const s = Math.max(0, Math.floor((Date.now() - createdMs) / 1000));
  if (s >= 86400) return `${Math.floor(s / 86400)}d`;
  if (s >= 3600) return `${Math.floor(s / 3600)}h`;
  if (s >= 60) return `${Math.floor(s / 60)}m`;
  return `${s}s`;
}

function fmtVal(v) {
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : v.toPrecision(4);
  return String(v);
}

/// train-manifest.json → loss 序列 [(x, y)]。兼容形态：
/// `loss: [v..]`、`loss: {step: v}`、`loss: [{step|x, loss|y}]`、
/// `loss_curve|history|metrics.loss` 同规则；标量 loss 返回单点。
function lossSeries(manifest) {
  const candidates = [
    manifest.loss,
    manifest.loss_curve,
    manifest.history,
    manifest.metrics?.loss,
    manifest.metrics?.loss_curve,
  ];
  for (const c of candidates) {
    const s = toSeries(c);
    if (s.length) return s;
  }
  return [];
}

function toSeries(c) {
  if (typeof c === "number") return [[0, c]];
  if (Array.isArray(c)) {
    if (c.every((x) => typeof x === "number")) return c.map((v, i) => [i, v]);
    const pairs = c
      .map((p, i) => {
        if (Array.isArray(p) && p.length >= 2) return [Number(p[0]), Number(p[1])];
        if (p && typeof p === "object") {
          const x = p.step ?? p.x ?? i;
          const y = p.loss ?? p.y ?? p.value;
          return [Number(x), Number(y)];
        }
        return null;
      })
      .filter((p) => p && Number.isFinite(p[0]) && Number.isFinite(p[1]));
    return pairs;
  }
  if (c && typeof c === "object") {
    return Object.entries(c)
      .map(([k, v]) => [Number(k), Number(v)])
      .filter((p) => Number.isFinite(p[0]) && Number.isFinite(p[1]))
      .sort((a, b) => a[0] - b[0]);
  }
  return [];
}

/// 无依赖 SVG sparkline：折线 + min/max 标注。
function sparkline(series) {
  const W = 360;
  const H = 80;
  const PAD = 6;
  const xs = series.map((p) => p[0]);
  const ys = series.map((p) => p[1]);
  const x0 = Math.min(...xs);
  const x1 = Math.max(...xs);
  const y0 = Math.min(...ys);
  const y1 = Math.max(...ys);
  const sx = (x) => PAD + (x1 === x0 ? W / 2 : ((x - x0) / (x1 - x0)) * (W - 2 * PAD));
  const sy = (y) => H - PAD - (y1 === y0 ? H / 2 : ((y - y0) / (y1 - y0)) * (H - 2 * PAD));
  const pts = series.map(([x, y]) => `${sx(x).toFixed(1)},${sy(y).toFixed(1)}`).join(" ");
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  svg.setAttribute("width", String(W));
  svg.setAttribute("height", String(H));
  svg.style.display = "block";
  svg.style.background = "#0b0e12";
  svg.style.border = "1px solid var(--border)";
  svg.style.borderRadius = "8px";
  const line = document.createElementNS(NS, "polyline");
  line.setAttribute("points", pts);
  line.setAttribute("fill", "none");
  line.setAttribute("stroke", "#79c0ff");
  line.setAttribute("stroke-width", "1.5");
  svg.append(line);
  const label = (x, y, text, anchor) => {
    const t = document.createElementNS(NS, "text");
    t.setAttribute("x", String(x));
    t.setAttribute("y", String(y));
    t.setAttribute("text-anchor", anchor);
    t.setAttribute("fill", "#8b98a9");
    t.setAttribute("font-size", "9");
    t.textContent = text;
    svg.append(t);
  };
  label(PAD, 10, `max ${fmtVal(y1)}`, "start");
  label(PAD, H - 2, `min ${fmtVal(y0)}`, "start");
  label(W - PAD, H - 2, `n=${series.length}`, "end");
  return el("div", {}, [el("div", { class: "muted", style: "font-size:.8rem" }, "loss"), svg]);
}

function errText(e) {
  return e?.message || "request failed";
}
