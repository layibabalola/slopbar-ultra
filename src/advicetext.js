// The plan advisor's card and the card of computers that could share one plan, as Settings >
// Subscriptions shows them. Pure: the report, the snapshot and which providers are on come in, HTML
// comes out, so `scripts/test-advice.mjs` can draw them from made-up accounts and computers.
import { t, shortDate, placesText } from "./i18n.js";
import { money, pct, amount } from "./accounttext.js";

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
const LINK = `<svg viewBox="0 0 16 16"><path d="M6 3h7v7M13 3 5 11"/></svg>`;

const advisedAccount = (a) => a.alias || a.email || t("unknownAccount");

/** The computers a plan is in use on now, as Accounts names them (this PC unnamed while sharing is off). */
export function computersOn(snapshot, provider, id) {
  const used = snapshot?.accounts?.find((x) => x.provider === provider && x.id === id);
  if (!used) return [];
  const { machines } = placesText(used);
  return machines.length ? machines : used.active || used.desktop ? [t("thisPcUnnamed")] : [];
}

/** One move of the advisor: what to do with which plan, what it saves, and why. */
export function adviceMove(m, snapshot) {
  const host = m.share ? advisedAccount(m.share) : "";
  const what = m.action === "cancel" ? t("advCancel", { plan: m.from })
    : m.action === "share" ? t("advShare", { account: host, plan: m.from })
    : t("advDowngrade", { from: m.from, to: m.to });
  const why = [t(`subVerdict_${m.verdict}`), m.peak_percent != null ? t("advPeak", { p: pct(m.peak_percent) }) : "",
    m.basis === "local_codex" ? t("advLocal") : "", m.before ? t("advBefore", { d: shortDate(m.before) }) : ""].filter(Boolean).join(" · ");
  // A share: how full the plan kept gets, and which computers move to it.
  const moving = m.share ? computersOn(snapshot, m.provider, m.id) : [];
  const share = m.share ? [t("advShareFit", { p: pct(m.share.combined_percent), plan: m.share.plan }),
    moving.length ? t("advShareSwitch", { list: moving.join(", "), account: host }) : "", t("advShareSession")].filter(Boolean).join(" · ") : "";
  return `
    <div class="advice-move">
      <div class="who"><div class="name">${esc(advisedAccount(m))}</div><small>${esc(what)} · <b>${esc(t("perMonth", { v: money(m.monthly_saving) }))}</b></small><small>${esc(why)}</small>${share ? `<small>${esc(share)}</small>` : ""}</div>
      <button class="btn use" data-billing="${esc(m.provider)}">${t("subManage")} ${LINK}</button>
    </div>`;
}

/** Too little capacity: why, and the cheapest fix (the advisor gives one only with no moves). */
export function shortageText(a, name) {
  const s = a.shortage;
  if (!s) return null;
  // With too little history the demand is this week's use so far, not the busiest weeks.
  const head = s.cause === "over_capacity" ? t(a.evidence === "clear_only" ? "advShortOverNow" : "advShortOver", { provider: name, n: amount(s.shortfall), unit: a.unit })
    : t("advShortLimits", { provider: name, n: s.exhausted });
  const f = s.fix;
  const fix = f.action === "upgrade" ? t("advFixUpgrade", { account: advisedAccount(f), from: f.from, plan: f.to, v: money(f.monthly_cost) })
    : t("advFixAdd", { count: f.count, plan: f.to, v: money(f.monthly_cost) });
  return { head, fix };
}

/**
 * The advisor: per provider, what the moves save (or what is short and its cheapest fix), what is
 * left, the plans with room for more use, and each move. `on(provider)` says whether it is turned
 * on, `names` maps providers to their names.
 */
export function adviceCard(report, { on, names, snapshot }) {
  const list = (report?.advice ?? []).filter((a) => on(a.provider));
  if (!list.length) return "";
  return `
    <div class="provider advisor">
      <div class="provider-head"><h3>${t("advTitle")}</h3></div>
      ${list.map((a) => {
        const name = names[a.provider] ?? a.provider;
        const lines = [];
        if (a.evidence === "clear_only") lines.push(t("advThin", { n: a.weeks_used, needed: a.weeks_needed }));
        const short = a.moves.length ? null : shortageText(a, name);
        const head = a.moves.length ? t("advSave", { provider: name, v: money(a.monthly_saving), n: a.moves.length }) : short ? short.head : t("advNone", { provider: name });
        if (short) lines.push(short.fix);
        if (a.spare_after != null) lines.push(t("advAfter", { held: amount(a.capacity_after), unit: a.unit, spare: a.spare_after_percent != null ? pct(a.spare_after_percent) : "—" }));
        if (a.kept_for_capacity) lines.push(t("advKept", { n: a.kept_for_capacity }));
        const room = (a.room ?? []).map((r) => t("advRoomItem", { account: advisedAccount(r), p: pct(r.free_percent) }));
        return `
          <p class="hint sub-pool"><b>${esc(head)}</b>${lines.length ? ` · ${esc(lines.join(" · "))}` : ""}</p>
          ${room.length ? `<p class="hint sub-pool">${esc(t("advRoom", { list: room.join(", ") }))}</p>` : ""}
          ${a.moves.length ? `<div class="group">${a.moves.map((m) => adviceMove(m, snapshot)).join("")}</div>` : ""}`;
      }).join("")}
      <p class="hint">${t("advHint")}</p>
    </div>`;
}

/**
 * Each computer's own use per provider (its busiest week in % of one plan, this week, how many weeks
 * are measured) and the groups of computers that could share one plan (the app's `machine_load`).
 */
export function machinesCard(report, { on, names }) {
  const list = (report?.machines ?? []).filter((m) => on(m.provider) && m.computers?.length);
  if (!list.length) return "";
  return `
    <div class="provider machines">
      <div class="provider-head"><h3>${t("mcTitle")}</h3></div>
      ${list.map((m) => {
        const name = names[m.provider] ?? m.provider;
        const label = (c) => (c.here ? (c.machine ? t("thisPc", { machine: c.machine }) : t("thisPcUnnamed")) : c.machine);
        // Group members come as names; this PC is marked as in the list of computers.
        const named = (n) => { const c = m.computers.find((x) => String(x.machine).toLowerCase() === String(n).toLowerCase()); return c ? label(c) : n; };
        // Computers that can be grouped: measured, with no use on plans of unknown size.
        const measured = m.computers.filter((c) => c.measured && !c.uncounted).length;
        const verdict = m.groups?.length ? m.groups.map((g) => t("mcGroup", { list: g.machines.map(named).join(", "), unit: m.unit, p: pct(g.combined_percent) }))
          : [measured >= 2 ? t("mcNone", { unit: m.unit }) : t("mcWait", { needed: m.weeks_needed, n: measured })];
        const rows = m.computers.map((c) => {
          const parts = [];
          if (c.busiest_percent != null) parts.push(t("mcBusiest", { p: pct(c.busiest_percent) }));
          if (c.live_percent != null) parts.push(t("mcLive", { p: pct(c.live_percent) }));
          // Enough weeks but not the latest: measured needs the last completed week too.
          if (!c.measured) parts.push(c.weeks >= m.weeks_needed ? t("mcLatest") : t("mcWeeks", { n: c.weeks, needed: m.weeks_needed }));
          if (c.needs_own) parts.push(t("mcOwn", { unit: m.unit }));
          if (c.uncounted) parts.push(t("mcUncounted"));
          return `
            <div class="advice-move">
              <div class="who"><div class="name">${esc(label(c))}</div><small>${esc(parts.join(" · ") || "—")}</small></div>
            </div>`;
        }).join("");
        return `
          <p class="hint sub-pool"><b>${esc(t("mcHead", { provider: name, unit: m.unit }))}</b></p>
          ${verdict.map((v) => `<p class="hint sub-pool">${esc(v)}</p>`).join("")}
          <div class="group">${rows}</div>`;
      }).join("")}
      <p class="hint">${t("mcHint")}</p>
    </div>`;
}
