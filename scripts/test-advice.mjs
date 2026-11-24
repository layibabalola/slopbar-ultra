// Synthetic checks of Settings > Subscriptions' advisor card and its card of computers that could
// share one plan (src/advicetext.js). Plain node, no browser: `node scripts/test-advice.mjs`. Every
// account and computer here is made up; nothing contacts a provider.
import assert from "node:assert/strict";

// i18n.js sets the page language on load.
globalThis.document ??= { documentElement: {} };
const { setLanguage, setThisMachine, setSharing } = await import("../src/i18n.js");
const { adviceCard, machinesCard, adviceMove, computersOn } = await import("../src/advicetext.js");
setLanguage("en");
setThisMachine("DESK-1");
setSharing({ presence_remote: "synthetic-remote" });

let passed = 0;
const test = (name, fn) => {
  try { fn(); passed++; } catch (err) { console.error(`FAIL ${name}\n${err.stack}`); process.exitCode = 1; }
};

const names = { claude: "Claude", codex: "Codex" };
const on = () => true;
// The visible text of some HTML, one line per element, as a person reads it.
const text = (html) => html.replace(/<[^>]+>/g, "\n").split("\n").map((l) => l.trim()).filter(Boolean)
  .map((l) => l.replace(/&#(\d+);/g, (_, c) => String.fromCharCode(Number(c)))).join("\n");

const now = Date.now();
const iso = (ms) => new Date(ms).toISOString();
const remote = (machine) => ({ machine, where: ["Windows"], seen_at: iso(now), live: true });
const account = (id, extra = {}) => ({ provider: "claude", id, email: `${id}@example.invalid`, alias: null, active: false, desktop: false, sources: [], remote: [], ...extra });

const move = (id, action, extra = {}) => ({ provider: "claude", id, email: `${id}@example.invalid`, alias: null, action, from: "Max 20x", to: null,
  monthly_saving: 200, verdict: "underused", basis: "observed", peak_percent: 20, before: null, manage_url: "https://claude.ai/settings/billing", ...extra });

const advice = (extra = {}) => ({ provider: "claude", evidence: "pool", unit: "Max 20x", weeks_used: 5, weeks_needed: 2, capacity_now: 3, capacity_after: 1,
  demand_p90: 0.5, floor: 0.6, moves: [], monthly_saving: 0, spare_after: 0.5, spare_after_percent: 100, kept_for_capacity: 0, shortage: null, room: [], ...extra });

test("a share names the plan to use, how full it gets, and the computers to switch", () => {
  const snapshot = { accounts: [account("light", { remote: [remote("LAPTOP-2")] }), account("busy", { active: true, sources: ["Windows"] })] };
  const share = move("light", "share", { share: { id: "busy", email: "busy@example.invalid", alias: null, plan: "Max 20x", combined_percent: 50 } });
  const lines = text(adviceMove(share, snapshot)).split("\n");
  assert.deepEqual(lines.slice(0, 4), [
    "light@example.invalid",
    "Use busy@example.invalid instead, then cancel Max 20x ·",
    "$200/month",
    "Mostly idle · busiest week 20%",
  ]);
  assert.equal(lines[4], "together 50% of its Max 20x at their busiest · switch LAPTOP-2 to busy@example.invalid first · their 5-hour limit is shared too (not checked)");
  // This PC is named while sharing is on, unnamed otherwise.
  assert.deepEqual(computersOn({ accounts: [account("x", { active: true })] }, "claude", "x"), ["DESK-1 (this PC)"]);
  setSharing({});
  assert.deepEqual(computersOn({ accounts: [account("x", { active: true })] }, "claude", "x"), ["this PC"]);
  setSharing({ presence_remote: "synthetic-remote" });
  // A plan in use nowhere says nothing about switching.
  assert.ok(!text(adviceMove(share, { accounts: [] })).includes("switch"));
});

test("cancels and downgrades read as before, and nothing is left unescaped", () => {
  const html = adviceMove(move("odd<b>", "downgrade", { to: "Max 5x", monthly_saving: 100, email: "a<b>@example.invalid" }), { accounts: [] });
  assert.ok(html.includes("a&#60;b&#62;@example.invalid") && !html.includes("<b>@"));
  assert.ok(text(html).includes("Move Max 20x → Max 5x ·"));
  assert.ok(text(adviceMove(move("idle", "cancel", { verdict: "unused" }), { accounts: [] })).includes("Cancel Max 20x ·"));
});

test("the advisor card totals the moves, lists room for more use, and shows a shortage's cheapest fix", () => {
  const card = text(adviceCard({ advice: [advice({ moves: [move("light", "share", { share: { id: "busy", email: "busy@example.invalid", alias: "Busy", plan: "Max 20x", combined_percent: 50 } })],
    monthly_saving: 200, room: [{ id: "busy", email: "busy@example.invalid", alias: "Busy", plan: "Max 20x", busiest_percent: 50, free_percent: 33.33 }] })] }, { on, names, snapshot: { accounts: [] } }));
  assert.ok(card.includes("Claude: save $200/month, changes: 1"), card);
  assert.ok(card.includes("Room for more use: Busy (about 33% of its plan free)"), card);
  assert.ok(card.includes("Use Busy instead, then cancel Max 20x ·"), card);
  const short = text(adviceCard({ advice: [advice({ capacity_now: 8, capacity_after: 8, spare_after: 0.03, spare_after_percent: 0.38, kept_for_capacity: 1,
    shortage: { cause: "over_capacity", shortfall: 1.56, exhausted: 6, fix: { action: "add", id: null, email: null, alias: null, from: null, to: "Max 20x", count: 2, monthly_cost: 400, capacity_after: 10 } } })] },
    { on, names, snapshot: { accounts: [] } }));
  assert.ok(short.includes("Claude: your busiest weeks plus 20% need 1.56 × Max 20x more than you hold"), short);
  assert.ok(short.includes("cheapest fix: add 2 × Max 20x, $400/month"), short);
  assert.ok(!short.includes("no plan changes advised"), short);
  const upgrade = text(adviceCard({ advice: [advice({ shortage: { cause: "hitting_limits", shortfall: 0.5, exhausted: 2,
    fix: { action: "upgrade", id: "b", email: "b@example.invalid", alias: null, from: "Max 5x", to: "Max 20x", count: 1, monthly_cost: 100, capacity_after: 2 } } })] }, { on, names, snapshot: { accounts: [] } }));
  assert.ok(upgrade.includes("Claude: plans keep reaching their weekly limit with under 40% to spare (2 of them)"), upgrade);
  assert.ok(upgrade.includes("cheapest fix: move b@example.invalid from Max 5x to Max 20x, $100/month more"), upgrade);
  // With too little history the shortage is against this week's use so far.
  const thin = text(adviceCard({ advice: [advice({ evidence: "clear_only", weeks_used: 1, shortage: { cause: "over_capacity", shortfall: 0.2, exhausted: 0,
    fix: { action: "add", id: null, email: null, alias: null, from: null, to: "Max 5x", count: 1, monthly_cost: 100, capacity_after: 1.25 } } })] }, { on, names, snapshot: { accounts: [] } }));
  assert.ok(thin.includes("Claude: this week's use so far plus 20% needs 0.2 × Max 20x more than you hold"), thin);
  // A provider turned off is left out, and an empty report draws nothing.
  assert.equal(adviceCard({ advice: [advice()] }, { on: () => false, names, snapshot: null }), "");
  assert.equal(adviceCard({}, { on, names, snapshot: null }), "");
});

test("the computers card names each computer's busiest week and the groups that could share one plan", () => {
  const computer = (machine, extra = {}) => ({ machine, here: false, busiest_percent: 30, live_percent: 10, weeks: 5, measured: true, needs_own: false, uncounted: false, seen_at: iso(now), ...extra });
  const report = { machines: [{ provider: "claude", unit: "Max 20x", weeks_needed: 4, computers: [
    computer("DESK-1", { here: true, busiest_percent: 25 }), computer("LAPTOP-2"), computer("TOWER-3", { busiest_percent: 95, needs_own: true }),
    computer("NEW-4", { weeks: 1, measured: false, busiest_percent: null, live_percent: 5, uncounted: true }),
  ], groups: [{ machines: ["desk-1", "LAPTOP-2"], combined_percent: 55 }] }] };
  const card = text(machinesCard(report, { on, names })).split("\n");
  assert.equal(card[0], "Computers that could share");
  assert.equal(card[1], "Claude: each computer's busiest week, in % of one Max 20x");
  assert.equal(card[2], "DESK-1 (this PC), LAPTOP-2 could share one Max 20x: 55% together at their busiest");
  assert.deepEqual(card.slice(3, 11), [
    "DESK-1 (this PC)", "busiest week 25% · last 8 days 10%",
    "LAPTOP-2", "busiest week 30% · last 8 days 10%",
    "TOWER-3", "busiest week 95% · last 8 days 10% · needs its own Max 20x",
    "NEW-4", "last 8 days 5% · 1 of 4 weeks measured · some use not counted (a plan of unknown size or with no weekly limit, an account SlopBar does not have, or readings that failed until the week reset)",
  ]);
  // Before two computers are measured it says when groups will show; after, that none fit.
  const waiting = { machines: [{ ...report.machines[0], groups: [], computers: [computer("DESK-1", { here: true, measured: false, weeks: 2 })] }] };
  assert.ok(text(machinesCard(waiting, { on, names })).includes("Groups show once two computers have 4 weeks measured (0 so far)."));
  const none = { machines: [{ ...report.machines[0], groups: [] }] };
  assert.ok(text(machinesCard(none, { on, names })).includes("No two computers fit one Max 20x with 20% to spare."));
  // Enough weeks but not the latest one says so, not "5 of 4".
  const latest = { machines: [{ ...report.machines[0], groups: [], computers: [computer("DESK-1", { here: true, measured: false, weeks: 5 })] }] };
  assert.ok(text(machinesCard(latest, { on, names })).includes("last completed week not measured"));
  // A computer whose use is partly uncounted is never grouped, so it does not count toward "no two fit".
  const partial = { machines: [{ ...report.machines[0], groups: [], computers: [computer("DESK-1", { here: true }), computer("LAPTOP-2", { uncounted: true })] }] };
  assert.ok(text(machinesCard(partial, { on, names })).includes("Groups show once two computers have 4 weeks measured (1 so far)."));
  assert.equal(machinesCard({ machines: [] }, { on, names }), "");
  assert.equal(machinesCard(report, { on: () => false, names }), "");
});

console.log(`${passed} advice card checks passed`);
