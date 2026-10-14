// 概览页（/）：GET /api/doctor?profile= 检查矩阵可视化。
// 组头显示组内 worst 状态；check 行 ok 绿 / warn 黄 / fail 红；
// hint 非 null 渲染次行；fail 项带跳转（ep.* → /components，model.* → /models）。

import { api, badge, checkRow, clear, el, errorBox, pageHead } from "../app.js";

const WORST = { fail: 2, warn: 1, ok: 0 };

let profile = null; // 首次由报告回填（服务端已按 MOMO_PROFILE 决定默认）
let current = null;

export default async function render(root) {
  let disposed = false;

  const head = pageHead("概览", {
    profile: profile ?? "normal",
    onProfile: (p) => {
      profile = p;
      load();
    },
    onRefresh: () => load(),
  });
  const body = el("div", {});
  root.append(head, body);

  await load();
  return () => {
    disposed = true;
  };

  async function load() {
    clear(body);
    body.append(el("p", { class: "muted" }, "running doctor checks…"));
    try {
      const q = profile ? `?profile=${profile}` : "";
      const report = await api(`/api/doctor${q}`);
      if (disposed) return;
      current = report;
      if (!profile) {
        profile = report.profile;
        // 回填顶栏 profile 高亮
        head.replaceWith(
          pageHead("概览", {
            profile,
            onProfile: (p) => {
              profile = p;
              load();
            },
            onRefresh: () => load(),
          })
        );
      }
      draw(report);
    } catch (e) {
      if (disposed) return;
      clear(body);
      body.append(errorBox(e));
    }
  }

  function draw(report) {
    clear(body);

    // 顶栏 summary 计数
    const s = report.summary;
    body.append(
      el("div", { class: "page-head", style: "margin-bottom:1rem" }, [
        el("span", { class: "muted" }, `profile: ${report.profile}`),
        badge("ok", `${s.ok} ok`),
        badge("warn", `${s.warn} warn`),
        badge("fail", `${s.fail} fail`),
      ])
    );

    if (!report.groups?.length) {
      body.append(el("p", { class: "muted" }, "empty report"));
      return;
    }

    const grid = el("div", { class: "grid" });
    for (const g of report.groups) {
      const worst =
        g.checks.reduce(
          (acc, c) => (WORST[c.status] > WORST[acc] ? c.status : acc),
          "ok"
        ) || "ok";
      const card = el(
        "section",
        { class: "card" },
        el("h2", {}, g.name, badge(worst))
      );
      for (const c of g.checks) {
        card.append(checkRow(c, jumpFor(c)));
      }
      grid.append(card);
    }
    body.append(grid);
  }

  // fail 项跳转：ep.* → /components，model.* → /models（issue #53 口径）。
  function jumpFor(check) {
    if (check.status !== "fail") return null;
    if (check.id.startsWith("ep."))
      return { href: "/components", label: "→ 组件页" };
    if (check.id.startsWith("model."))
      return { href: "/models", label: "→ 模型页" };
    return null;
  }
}
