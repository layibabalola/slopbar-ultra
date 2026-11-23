// Synthetic checks of the flyout's account tooltip (src/accounttext.js) and sparkline scaling
// (src/sparkline.js). Plain node, no browser: `node scripts/test-account-tip.mjs`. Every account
// here is made up; nothing contacts a provider.
import assert from "node:assert/strict";

// i18n.js sets the page language on load.
globalThis.document ??= { documentElement: {} };
const { t, setLanguage, setThisMachine, setSharing, windowResetText, placesText, compactPlaces, overlapText, shownReading, duration, currentWindows, coveredMonitoring, resetSince } = await import("../src/i18n.js");
const { accountTip, accountTipLines, adviceText, cliSignInText } = await import("../src/accounttext.js");
const { sparkline, sparklineText } = await import("../src/sparkline.js");
setLanguage("en");
setThisMachine("DESK-1");
setSharing({ presence_remote: "synthetic-remote" });

let passed = 0;
const test = (name, fn) => {
  try { fn(); passed++; } catch (err) { console.error(`FAIL ${name}\n${err.stack}`); process.exitCode = 1; }
};

const HOUR = 3600000;
const now = Date.now();
const iso = (ms) => new Date(ms).toISOString();

// ---------------------------------------------------------------------------------------------
// Sparklines

const pathOf = (svg) => /class="spark-line" d="([^"]*)"/.exec(svg)?.[1] ?? "";
const ys = (d) => [...d.matchAll(/[ML][\d.]+ ([\d.]+)/g)].map((m) => Number(m[1]));

test("no readings draw nothing, keep their place and say so", () => {
  for (const points of [undefined, null, []]) {
    const html = sparkline(points, { now });
    assert.match(html, /class="spark spark-empty"/);
    assert.match(html, /width:56px;height:16px/);
    assert.ok(!html.includes("spark-base") && !html.includes("<svg"), "no flat baseline");
    assert.ok(html.includes(`aria-label="${t("sparkNone")}"`));
  }
});

test("a silent sparkline keeps only its place and says nothing", () => {
  const html = sparkline([[now - HOUR, 40]], { now, silent: true });
  assert.match(html, /aria-hidden="true"/);
  assert.match(html, /width:56px;height:16px/);
  for (const said of ["role=", "aria-label", "title", "<svg"]) assert.ok(!html.includes(said), said);
});

test("readings older than 7 days count as none", () => {
  const html = sparkline([[now - 8 * 24 * HOUR, 40]], { now });
  assert.match(html, /spark-empty/);
});

test("a flat 0% week is drawn along the bottom, not as an empty line", () => {
  const pts = [0, 1, 2, 3].map((h) => [now - (3 - h) * HOUR, 0]);
  const svg = sparkline(pts, { now, height: 16 });
  const d = pathOf(svg);
  assert.ok(d.includes("L"), "the readings are joined");
  assert.deepEqual([...new Set(ys(d))], [14.5], "at 0%, inside the padding");
  assert.match(svg, /class="spark-dot ok"/);
  assert.ok(svg.includes(sparklineText(pts)));
});

test("percentages scale onto the height, clamped to 0–100", () => {
  const pts = [[now - 3 * HOUR, 100], [now - 2 * HOUR, 50], [now - HOUR, 150], [now, -5]];
  const d = pathOf(sparkline(pts, { now, height: 16 }));
  assert.deepEqual(ys(d), [1.5, 8, 1.5, 14.5]);
  // The last reading sits at the right edge.
  assert.match(sparkline(pts, { now, width: 56 }), /cx="54.5"/);
});

test("readings hours apart are a gap, not a slope", () => {
  const d = pathOf(sparkline([[now - 10 * HOUR, 10], [now - HOUR, 20]], { now }));
  assert.equal((d.match(/M/g) ?? []).length, 2);
});

test("the row's own tooltip can stand in for the sparkline's", () => {
  const pts = [[now - HOUR, 10], [now, 20]];
  assert.ok(sparkline(pts, { now }).includes("<title>"));
  assert.ok(!sparkline(pts, { now, tooltip: false }).includes("<title>"));
  assert.ok(!sparkline([], { now, tooltip: false }).includes("title="));
});

// ---------------------------------------------------------------------------------------------
// The tooltip

const weekly = { kind: "weekly", used_percent: 77, window_seconds: 604800, resets_at: iso(now + 52 * HOUR) };
const opus = { kind: "weekly_opus", used_percent: 40, window_seconds: 604800, resets_at: iso(now + 52 * HOUR) };
const account = {
  id: "fixture-1", provider: "claude", email: "max@example.invalid", alias: "Work", plan: "Max 20x", org: null,
  active: true, sources: ["Windows"], windows: [weekly, opus], credits: null, error: null, monitor_error: null,
  stale: false, fetched_at: iso(now - 3 * 60000), monitor_connected: true, used_outside: true, leased: "nightly build",
  runs_out_at: iso(now + 30 * HOUR),
  remote: [{ machine: "LAPTOP-2", where: ["WSL: Ubuntu"], seen_at: iso(now), live: true },
    { machine: "DESK-3", where: ["Windows"], seen_at: iso(now - 2 * HOUR), live: false }],
  resets: [{ kind: "weekly", count: 2, usable: true, title: null, expires_at: iso(now + 20 * 24 * HOUR), next_available_at: null }],
};
const sub = {
  provider: "claude", id: "fixture-1", plan_kind: "personal", monthly_cost: 200, cost_entered: false,
  renewal: { at: iso(now + 5 * 24 * HOUR), source: "provider" }, ends_at: null, cancel_after: null,
  assessment: { verdict: "unused", renew_wasteful: true }, idle_now: false, delinquent: false,
  this_week: { used_percent: 77, pace_percent: 140, resets_at: weekly.resets_at, runs_out_at: null },
  credits: null, extra_usage: null,
};

test("a full account says everything the full card does, grouped", () => {
  const groups = accountTipLines(account, { sub, trail: [[now - HOUR, 70], [now, 77]], creditAdvice: "Spend it now", now });
  assert.equal(groups.length, 5);
  const [who, usage, state, held, plan] = groups;
  assert.equal(who[0], "Work (max@example.invalid)");
  assert.equal(who[1], "Claude · Max 20x");
  assert.equal(who[2], "In use on DESK-1 (this PC), LAPTOP-2 (WSL: Ubuntu)");
  assert.ok(who.some((l) => l.startsWith("DESK-3:")), "machines that went quiet");
  assert.ok(who.includes(t("usedOutside")) && who.includes(t("leased", { purpose: "nightly build" })));
  // Each window, model-specific ones too, with its reset relative and absolute, as the full card words it.
  assert.equal(usage[0], `${t("weekly")}: 77% ${t("used")} · ${windowResetText(weekly, false, account.fetched_at)}`);
  assert.match(usage[0], /Resets in 2d 4h · /);
  assert.ok(usage[1].startsWith(`${t("weekly_opus")}: 40% ${t("used")} · `));
  assert.ok(usage.some((l) => l.startsWith(t("runsOut", { at: "" }).trim())));
  assert.ok(usage.includes(t("tipPace", { p: 140 })));
  assert.ok(usage.includes(sparklineText([[now - HOUR, 70], [now, 77]])));
  assert.deepEqual(state, [t("lastObserved", { t: "3m" }), t("tipMonitorOn")]);
  assert.ok(held.some((l) => l.includes(t("resetUsableNow"))) && held.includes("Spend it now"));
  assert.ok(plan[0].includes("$200") && plan[0].includes(t("subSourceProvider")), plan[0]);
  assert.ok(plan.includes(t("tipVerdict", { v: t("subVerdict_unused") })));
  assert.ok(plan.includes(adviceText(sub)) && adviceText(sub));
});

test("the tooltip is plain lines with one blank line between groups", () => {
  const tip = accountTip(account, { sub, now });
  assert.ok(!tip.includes("\n\n\n") && !tip.startsWith("\n") && !tip.endsWith("\n"));
  assert.equal(tip.split("\n\n").length, 5);
});

test("missing fields are left out, not shown empty", () => {
  const bare = { id: "fixture-2", provider: "codex", email: "solo@example.invalid", alias: null, plan: null, org: null,
    active: false, sources: [], windows: [], credits: null, error: null, stale: false, fetched_at: null,
    monitor_connected: false, resets: [] };
  assert.deepEqual(accountTipLines(bare, { now }), [["solo@example.invalid", "Codex"], [t("tipMonitorOff")]]);
});

// ---------------------------------------------------------------------------------------------
// Machines an account is in use on

const laptop = { machine: "LAPTOP-2", where: ["WSL: Ubuntu"], seen_at: iso(now), live: true };
const used = (over) => ({ active: false, desktop: false, sources: [], remote: [], overlap: [], ...over });

test("every machine an account is in use on is named, this one first and marked", () => {
  const cases = [
    [used({ active: true, sources: ["Windows"] }), "In use on DESK-1 (this PC)", ["DESK-1 (this PC)"]],
    [used({ active: true, desktop: true, sources: ["Windows"] }), "In use on DESK-1 (this PC, Windows, Desktop app)", ["DESK-1 (this PC)"]],
    [used({ desktop: true }), "In use on DESK-1 (this PC, Desktop app)", ["DESK-1 (this PC)"]],
    [used({ active: true, sources: ["Windows"], remote: [laptop] }), "In use on DESK-1 (this PC), LAPTOP-2 (WSL: Ubuntu)", ["DESK-1 (this PC)", "LAPTOP-2"]],
    [used({ remote: [laptop] }), "In use on LAPTOP-2 (WSL: Ubuntu)", ["LAPTOP-2"]],
    [used({ remote: [{ ...laptop, live: false }] }), "", []],
    [used({}), "", []],
  ];
  for (const [a, inUse, machines] of cases) {
    const p = placesText(a);
    assert.equal(p.inUse, inUse);
    assert.deepEqual(p.machines, machines);
    assert.equal(p.short, machines.length ? `In use on ${machines.join(", ")}` : "", "chips leave the places to their tooltip");
    assert.deepEqual(p.names, machines.map((m) => m.replace(" (this PC)", "")));
  }
  assert.ok(placesText(used({ remote: [{ ...laptop, live: false }] })).seen[0].startsWith("LAPTOP-2 (WSL: Ubuntu): seen "));
});

test("without in-use sharing this machine goes unnamed, and nothing is marked", () => {
  setSharing({ presence_folder: null, presence_remote: null });
  try {
    const p = placesText(used({ active: true, desktop: true, sources: ["Windows"] }));
    assert.equal(p.inUse, "In use on this PC (Windows, Desktop app)");
    assert.deepEqual(p.names, [], "no name, so no compact line and the old chips");
    assert.equal(overlapText(used({ overlap: ["DESK-1", "LAPTOP-2"] })).lines[0], "In use on 2 machines at once: DESK-1, LAPTOP-2");
  } finally {
    setSharing({ presence_remote: "synthetic-remote" });
  }
  assert.deepEqual(placesText(used({ active: true, sources: ["Windows"] })).names, ["DESK-1"]);
});

test("an unknown name for this machine still says this PC", () => {
  setThisMachine("");
  try {
    assert.equal(placesText(used({ active: true, sources: ["Windows"] })).inUse, "In use on this PC");
    assert.equal(placesText(used({ active: true, sources: ["WSL: Ubuntu"], remote: [laptop] })).inUse, "In use on this PC (WSL: Ubuntu), LAPTOP-2 (WSL: Ubuntu)");
    assert.deepEqual(placesText(used({ active: true })).names, []);
    const both = placesText(used({ active: true, sources: ["Windows"], remote: [laptop] }));
    assert.deepEqual(both.machines, ["LAPTOP-2"], "the compact line never shows a bare (inflected) this PC");
    assert.equal(both.short, "In use on this PC, LAPTOP-2");
    assert.equal(overlapText(used({ active: true, overlap: ["?", "LAPTOP-2"] })).lines[0], "In use on 2 machines at once: this PC, LAPTOP-2");
    assert.deepEqual(overlapText(used({ overlap: ["DESK-1", "LAPTOP-2"] })).lines[0], "In use on 2 machines at once: DESK-1, LAPTOP-2");
  } finally {
    setThisMachine("DESK-1");
  }
});

test("an overlap marks this PC by its place, never by a name a clone may share", () => {
  const o = overlapText(used({ active: true, overlap: ["DESK-1", "LAPTOP-2"] }));
  assert.equal(o.chip, t("overlapChip", { n: 2 }));
  assert.equal(o.lines[0], "In use on 2 machines at once: DESK-1 (this PC), LAPTOP-2");
  assert.equal(overlapText(used({ overlap: ["DESK-1", "LAPTOP-2"] })).lines[0], "In use on 2 machines at once: DESK-1, LAPTOP-2",
    "not in use here: a machine named like this one is another machine");
});

test("another record under this machine's name is not listed twice", () => {
  const twin = { machine: "desk-1", where: ["WSL: Ubuntu"], seen_at: iso(now), live: true };
  const p = placesText(used({ active: true, sources: ["Windows"], remote: [twin, laptop] }));
  assert.equal(p.inUse, "In use on DESK-1 (this PC), LAPTOP-2 (WSL: Ubuntu)");
  assert.deepEqual(p.names, ["DESK-1", "LAPTOP-2"]);
  assert.equal(placesText(used({ remote: [twin] })).inUse, "In use on desk-1 (WSL: Ubuntu)", "not in use here: listed as it is");
});

test("the compact row names two machines in full, this one second, then how many more", () => {
  const on = (machine) => ({ machine, where: ["Windows"], seen_at: iso(now), live: true });
  const here = { active: true, sources: ["Windows"] };
  assert.equal(compactPlaces(used({ remote: [on("BOX-0001"), on("BOX-0002")] })), "BOX-0001, BOX-0002");
  assert.equal(compactPlaces(used(here)), "DESK-1 (this PC)");
  // Listed with others, this machine comes second: a narrow row cuts its mark, not a name.
  assert.equal(compactPlaces(used({ ...here, remote: [on("WORKSTATION")] })), "WORKSTATION, DESK-1 (this PC)");
  assert.equal(compactPlaces(used({ ...here, remote: [on("WORKSTATION"), on("BOX-0003")] })), "WORKSTATION, DESK-1 (this PC) +1");
  assert.equal(compactPlaces(used({ remote: [on("A-1"), on("B-2"), on("C-3"), on("D-4")] })), "A-1, B-2 +2");
  assert.equal(compactPlaces(used({})), "");
  // Two records under one name are one machine, as the app's overlap badge counts them.
  assert.equal(compactPlaces(used({ remote: [on("LAPTOP-2"), on("laptop-2"), on("OTHER-PC")] })), "LAPTOP-2, OTHER-PC");
  assert.equal(compactPlaces(used({ ...here, remote: [on("LAPTOP-2"), on("LAPTOP-2")] })), "LAPTOP-2, DESK-1 (this PC)");
  assert.deepEqual(placesText(used({ remote: [on("CLONE-VM"), on("CLONE-VM"), on("clone-vm")] })).machines, ["CLONE-VM"]);
  // The full line keeps this machine first.
  assert.equal(placesText(used({ ...here, remote: [on("WORKSTATION")] })).short, "In use on DESK-1 (this PC), WORKSTATION");
});

test("machine names are translated around, never translated themselves", () => {
  setLanguage("de");
  try {
    assert.equal(placesText(used({ active: true, desktop: true, sources: ["Windows"] })).inUse, "Aktiv auf DESK-1 (dieser PC, Windows, Desktop-App)");
  } finally {
    setLanguage("en");
  }
});

test("errors are worded with their fix, and stale readings say so", () => {
  const failing = { ...account, error: "token_expired", stale: true, runs_out_at: iso(now + HOUR) };
  const lines = accountTipLines(failing, { now }).flat();
  assert.ok(lines.includes(t("token_expired", { provider: "Claude Code" })));
  assert.ok(lines.includes(`${t("lastObserved", { t: "3m" })} · ${t("lastReading")}`));
  assert.ok(!lines.some((l) => l.startsWith(t("runsOut", { at: "" }).trim())), "no forecast from a stale reading");
});

test("a monitoring renewal the network's proxy stopped says it was monitoring", () => {
  const script = t("proxy_script");
  // Usage was read through the CLI's own sign-in; only monitoring's renewal was refused.
  const renewal = accountTipLines({ ...account, monitor_error: "proxy_script" }, { now }).flat();
  assert.ok(renewal.includes(t("monitorProxy", { reason: script })), renewal.join("\n"));
  // A usage error (a read, or a renewal with nothing to fall back on) is the proxy's words alone.
  const usage = accountTipLines({ ...account, error: "proxy_script", monitor_error: "proxy_script", stale: true }, { now }).flat();
  assert.ok(usage.includes(script) && !usage.includes(t("monitorProxy", { reason: script })), usage.join("\n"));
});

test("another computer's reading shows while this one has none fresh, marked with whose and how old", () => {
  const shared = { machine: "LAPTOP-2", taken_at: iso(now - 12 * 60000),
    windows: [{ kind: "weekly", window_seconds: null, used_percent: 61, resets_at: iso(now + 50 * HOUR) }] };
  const ended = { ...account, error: "monitor_reconnect", stale: true, fetched_at: iso(now - 72 * HOUR), shared };
  const reading = shownReading(ended, now);
  assert.equal(reading.from, t("readOn", { machine: "LAPTOP-2", t: duration(12) }));
  assert.deepEqual([reading.windows, reading.at, reading.stale], [shared.windows, shared.taken_at, false], "its reset is as of that reading");
  const lines = accountTipLines(ended, { now }).flat();
  assert.ok(lines.some((l) => l.startsWith(`${t("weekly")}: 61% ${t("used")} · `)), lines.join("\n"));
  assert.ok(!lines.some((l) => l.startsWith(`${t("weekly")}: ${Math.round(account.windows.find((w) => w.kind === "weekly")?.used_percent ?? -1)}%`)), "not this computer's old numbers");
  assert.ok(lines.includes(reading.from), "whose it is and how old");
  assert.ok(lines.includes(`${t("lastObservedHere", { t: duration(72 * 60) })} · ${t("lastReading")}`), "this computer's own reading stays said, as its own");
  assert.ok(lines.includes(t("monitor_reconnect", { provider: "Claude Code" })), "and its error");
  // No shared reading: this computer's own, unmarked.
  assert.deepEqual(shownReading(account, now), { windows: account.windows, at: account.fetched_at, stale: account.stale, from: "", shared: false });
  assert.ok(accountTipLines(account, { now }).flat().includes(t("lastObserved", { t: duration(3) })));
});

test("a window of another computer's reading that has reset since keeps its numbers and says so", () => {
  const session = { kind: "session", window_seconds: null, used_percent: 80, resets_at: iso(now - HOUR) };
  const weekly = { kind: "weekly", window_seconds: null, used_percent: 40, resets_at: iso(now + 50 * HOUR) };
  const shared = { machine: "LAPTOP-2", taken_at: iso(now - 3 * HOUR), windows: [session, weekly] };
  const a = { ...account, error: "monitor_reconnect", stale: true, shared };
  const reading = shownReading(a, now);
  assert.deepEqual(currentWindows(reading, now), [weekly], "the headline comes from windows that have not reset");
  assert.equal(windowResetText(session, false, reading.at, true), t("resetSinceReading", { d: windowResetText(session, true).replace(/^.*: /, "") }));
  const lines = accountTipLines(a, { now }).flat();
  assert.ok(lines.some((l) => l.startsWith(`${t("session")}: 80% ${t("used")} · ${t("resetSinceReading", { d: "" }).split("(")[0]}`)), lines.join(" | "));
  // All reset since: no number at all.
  assert.deepEqual(currentWindows(shownReading({ ...a, shared: { ...shared, windows: [session] } }, now), now), []);
  // This computer's own windows are never filtered.
  assert.equal(currentWindows(shownReading({ ...account, windows: [session] }, now), now).length, 1);
});

test("a monitoring sign-in another computer covers is no problem mark; any other error still is", () => {
  const covered = { ...account, covered_by: "LAPTOP-2" };
  assert.equal(coveredMonitoring({ ...covered, monitor_renewal: "ended", error: "monitor_reconnect", stale: true }), true);
  assert.equal(coveredMonitoring({ ...covered, monitor_connected: false, error: "monitor_setup", stale: true }), true);
  assert.equal(coveredMonitoring({ ...covered, monitor_renewal: "ended", monitor_error: "monitor_reconnect" }), true, "in use here, read through the CLI");
  for (const error of ["token_expired", "rate_limited", "http_500", "proxy_script"]) {
    assert.equal(coveredMonitoring({ ...covered, monitor_connected: false, error, stale: true }), false, error);
  }
  assert.equal(coveredMonitoring({ ...covered, monitor_renewal: null }), false, "nothing to make here");
  assert.equal(coveredMonitoring({ ...account, covered_by: null, monitor_renewal: "ended", error: "monitor_reconnect" }), false);
});

test("beside another computer's reading, this computer's trail is not described", () => {
  const shared = { machine: "LAPTOP-2", taken_at: iso(now - 12 * 60000), windows: [{ kind: "weekly", window_seconds: null, used_percent: 61, resets_at: iso(now + 50 * HOUR) }] };
  const trail = [[now - 2 * HOUR, 12], [now - HOUR, 48]];
  assert.ok(accountTipLines(account, { now, trail }).flat().includes(sparklineText(trail)));
  assert.ok(!accountTipLines({ ...account, stale: true, shared }, { now, trail }).flat().includes(sparklineText(trail)));
  // A Codex window not started when it was read is not "reset since", whatever its age.
  const unused = { kind: "session", window_seconds: 18000, used_percent: 0, resets_at: iso(now - 6 * HOUR + 18000 * 1000) };
  const old = shownReading({ ...account, stale: true, shared: { ...shared, taken_at: iso(now - 6 * HOUR), windows: [unused] } }, now);
  assert.deepEqual(currentWindows(old, now), [unused]);
  assert.equal(resetSince(old, unused, now), false);
});

test("a saved CLI sign-in that ended or ends soon says so before anyone presses Use", () => {
  const spare = { ...account, active: false, sources: [] };
  const ended = cliSignInText({ ...spare, cli_renewal: "ended", cli_ends_at: iso(now - 26 * HOUR) }, now);
  assert.equal(ended.ended, true);
  assert.equal(ended.label, t("cliSignInEnded"));
  assert.match(ended.hint, /30 days/);
  assert.match(ended.hint, /Use would stop the CLI/);
  const ending = cliSignInText({ ...spare, cli_renewal: "ending", cli_ends_at: iso(now + 50 * HOUR) }, now);
  assert.equal(ending.ended, false);
  assert.match(ending.label, /^CLI sign-in ends in 2d 2h$/);
  // By the date, not the last refresh's verdict: an end that passed since says so already.
  assert.equal(cliSignInText({ ...spare, cli_renewal: "ending", cli_ends_at: iso(now - 60000) }, now).ended, true);
  // Nothing to say: the account in use (even with the verdict from before a switch), no end known,
  // or an end more than 3 days away (Sign in all accounts still takes it from 7).
  assert.equal(cliSignInText({ ...account, cli_renewal: "ended", cli_ends_at: iso(now - HOUR) }, now), null);
  assert.equal(cliSignInText({ ...spare, cli_renewal: "ended" }, now), null);
  assert.equal(cliSignInText({ ...spare, cli_renewal: "ending", cli_ends_at: iso(now + 5 * 24 * HOUR) }, now), null);
  // An account known only by its token is never promised a sign-in it cannot get.
  const token = cliSignInText({ ...spare, id: "token:abc", cli_ends_at: iso(now - HOUR) }, now);
  assert.equal(token.ended, true);
  assert.equal(token.hint, t("cliRejectedHint", { cli: "claude /login" }));
  // The compact flyout's tooltip carries it with everything else.
  const lines = accountTipLines({ ...spare, cli_renewal: "ended", cli_ends_at: iso(now - HOUR) }, { now }).flat();
  assert.ok(lines.some((l) => l.startsWith(`${t("cliSignInEnded")}: `)), lines.join("\n"));
  // Every language has its own words for them, with the same placeholders.
  const keys = ["cliSignInEnded", "cliSignInEnding", "cliSignInEndedHint", "cliSignInEndingHint"];
  const english = Object.fromEntries(keys.map((k) => [k, t(k, { t: "{t}", d: "{d}" })]));
  try {
    for (const lang of ["pt", "es", "fr", "de", "it", "ja", "zh", "ko", "ru"]) {
      setLanguage(lang);
      for (const key of keys) {
        const text = t(key, { t: "{t}", d: "{d}" });
        assert.notEqual(text, english[key], `${lang} ${key} is translated`);
        for (const slot of ["{t}", "{d}"]) assert.equal(text.includes(slot), english[key].includes(slot), `${lang} ${key} keeps ${slot}`);
      }
    }
  } finally {
    setLanguage("en");
  }
});

console.log(`${passed} account tooltip and sparkline checks passed`);
