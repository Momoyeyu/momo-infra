// 模型页（/models）：MANIFEST 清单 + pull + 激活选择。
// 数据：GET /api/models（MANIFEST.toml × models.lock merge，逐模型
//       missing/partial/verified + active 标记 + active 选择快照）；
// 操作：POST /api/models/pull {name}——直跑等结果（逐文件进度流待 #54 jobs）。
// 激活选择只读展示：写回走 #56 /api/config 通路（未实装）。

import { api, badge, checkRow, clear, el, errorBox, pageHead } from "../app.js";

const STATE_LABEL = { missing: "missing", partial: "partial", verified: "verified" };
// state → badge 配色：verified 绿，partial 黄，missing 灰
const STATE_BADGE = { verified: "ok", partial: "warn", missing: "" };

export default async function render(root) {
  let disposed = false;
  let busy = false;
  const expanded = new Set();

  const head = pageHead("模型", { onRefresh: () => load() });
  const resultBox = el("div", {});
  const body = el("div", {});
  root.append(head, resultBox, body);

  await load();
  return () => {
    disposed = true;
  };

  async function load() {
    clear(body);
    body.append(el("p", { class: "muted" }, "loading…"));
    try {
      const report = await api("/api/models");
      if (disposed) return;
      draw(report);
    } catch (e) {
      if (disposed) return;
      clear(body);
      body.append(errorBox(e));
    }
  }

  function draw(report) {
    clear(body);

    for (const w of report.warn ?? []) {
      body.append(el("p", { class: "muted" }, `⚠ ${w}`));
    }

    // ---- 激活选择（只读；写回待 #56）----
    const a = report.active ?? {};
    const field = (label, v) =>
      el("div", { class: "check" }, [
        el("span", { class: "cid" }, label),
        el("span", { class: "detail mono", style: "grid-column: 2 / -1" },
          v ?? el("span", { class: "muted" }, "(unset)")),
      ]);
    body.append(
      el("section", { class: "card" }, [
        el("h2", {}, "激活选择", badge("", "read-only")),
        field("ASR", join(a.asr_provider, a.asr_model)),
        field("声线", join(a.tts_provider, a.tts_voice, a.tts_lang)),
        field("大脑", join(a.brain_model, a.brain_endpoint)),
        el("p", { class: "muted", style: "margin:.5rem 0 0;font-size:.8rem" },
          "切换写回待 #56 /api/config 通路实装；当前仅展示 env 快照。"),
      ])
    );

    // ---- MANIFEST 清单表 ----
    if (!report.models?.length) {
      body.append(
        el("p", { class: "muted" }, report.manifest
          ? "manifest has no entries"
          : "MANIFEST.toml not found")
      );
      return;
    }

    const rows = report.models.map((m) => {
      const nVerified = m.files.filter((f) => f.state === "verified").length;
      const tr = el("tr", { style: "cursor:pointer", onclick: () => toggle(m.name) }, [
        el("td", {}, [
          el("span", { class: "mono" }, m.name),
          " ",
          m.active ? badge("ok", "active") : null,
        ]),
        el("td", {}, badge(STATE_BADGE[m.state] ?? "", STATE_LABEL[m.state] ?? m.state)),
        el("td", { class: "mono muted" }, `${m.repo_id}@${m.revision}`),
        el("td", { class: "num muted" }, `${nVerified}/${m.files.length} files`),
        el("td", {}, el("button", {
          class: "btn",
          disabled: busy,
          onclick: (e) => {
            e.stopPropagation();
            pull(m.name);
          },
        }, "pull")),
      ]);
      const out = [tr];
      if (m.notes) {
        out.push(el("tr", {}, el("td", { colspan: "5", class: "muted", style: "font-size:.8rem" }, m.notes)));
      }
      if (m.warn?.length) {
        for (const w of m.warn) {
          out.push(el("tr", {}, el("td", { colspan: "5", class: "hint" }, w)));
        }
      }
      if (expanded.has(m.name)) {
        out.push(
          el("tr", {}, el("td", { colspan: "5" }, fileTable(m)))
        );
      }
      return out;
    });

    body.append(
      el("section", { class: "card" }, [
        el("h2", {}, "MANIFEST 清单", el("span", { class: "muted", style: "font-weight:400;font-size:.8rem" }, report.manifest ?? "")),
        el("table", { class: "list" }, [
          el("tr", {}, [
            el("th", {}, "name"), el("th", {}, "state"), el("th", {}, "repo"),
            el("th", {}, "files"), el("th", {}, ""),
          ]),
          rows,
        ]),
        el("p", { class: "muted", style: "font-size:.8rem;margin-bottom:0" },
          `models dir: ${report.models_dir}`),
      ])
    );
  }

  function fileTable(m) {
    return el("table", { class: "list", style: "font-size:.8rem" }, [
      el("tr", {}, [el("th", {}, "file"), el("th", {}, "state"), el("th", {}, "size")]),
      m.files.map((f) =>
        el("tr", {}, [
          el("td", { class: "mono" }, f.path),
          el("td", {}, [
            badge(f.state === "verified" ? "ok" : f.state === "unverified" ? "warn" : "fail", f.state),
            f.sha256_placeholder ? [" ", badge("warn", "sha256 TODO")] : null,
          ]),
          el("td", { class: "num muted" }, fmtSize(f.actual_size ?? f.expected_size)),
        ])
      ),
    ]);
  }

  function toggle(name) {
    if (expanded.has(name)) expanded.delete(name);
    else expanded.add(name);
    load();
  }

  async function pull(name) {
    busy = true;
    setButtons(true);
    clear(resultBox);
    resultBox.append(
      el("p", { class: "muted" }, `pulling ${name}…（同步直跑，大文件需等待；进度流待 #54）`)
    );
    try {
      const report = await api("/api/models/pull", {
        method: "POST",
        body: { name },
      });
      if (disposed) return;
      clear(resultBox);
      const card = el(
        "section",
        { class: "card" },
        el("h2", {}, `pull ${name}`, badge(report.summary.fail ? "fail" : "ok"))
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
      load();
    }
  }

  function setButtons(disabled) {
    for (const b of root.querySelectorAll("button.btn")) b.disabled = disabled;
  }
}

function join(...parts) {
  const s = parts.filter(Boolean).join(" · ");
  return s || null;
}

function fmtSize(n) {
  if (n === null || n === undefined) return "—";
  const units = ["B", "KiB", "MiB", "GiB"];
  let v = n, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${i === 0 ? v : v.toFixed(1)} ${units[i]}`;
}
