// Synthetic checks of the flyout's account order (src/accountorder.js): alphabetical, or the
// soonest weekly reset first. Plain node, no browser: `node scripts/test-account-order.mjs`. Every
// account here is made up; nothing contacts a provider.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// i18n.js sets the page language on load.
globalThis.document ??= { documentElement: {} };
const { setLanguage } = await import("../src/i18n.js");
const { orderAccounts, weeklyResetAt } = await import("../src/accountorder.js");
setLanguage("en");

let passed = 0;
const test = (name, fn) => {
  try { fn(); passed++; } catch (err) { console.error(`FAIL ${name}\n${err.stack}`); process.exitCode = 1; }
};

const MINUTE = 60000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const now = Date.parse("2026-11-26T05:00:00Z");
const iso = (ms) => new Date(ms).toISOString();
const fetched = iso(now - 2 * MINUTE);

/** A Claude account: its weekly limit only, as Claude reports it (no window length). */
const claude = (email, resetsIn, used = 40, extra = {}) => ({
  provider: "claude", id: `c-${email}`, email, fetched_at: fetched, stale: false,
  windows: [{ kind: "weekly", window_seconds: null, used_percent: used, resets_at: resetsIn == null ? null : iso(now + resetsIn) }], ...extra,
});
/** A Codex account with a weekly limit under way. */
const codex = (email, resetsIn, used = 20) => ({
  provider: "codex", id: `x-${email}`, email, fetched_at: fetched, stale: false,
  windows: [{ kind: "weekly", window_seconds: 604800, used_percent: used, resets_at: iso(now + resetsIn) }],
});
/** A Codex week not started: nothing used, and the time said is a whole week after the reading. */
const idle = (email) => ({
  provider: "codex", id: `x-${email}`, email, fetched_at: fetched, stale: false,
  windows: [{ kind: "weekly", window_seconds: 604800, used_percent: 0, resets_at: iso(Date.parse(fetched) + 7 * DAY + 3000) }],
});
const emails = (list) => list.map((a) => a.email);

test("alphabetical, the default, keeps the app's order", () => {
  const list = [claude("a@example.test", 5 * DAY), claude("b@example.test", HOUR), claude("c@example.test", 3 * DAY)];
  for (const order of ["alphabetical", undefined, null, "from_the_future"]) {
    assert.equal(orderAccounts(list, order, now), list, String(order));
  }
});

test("by weekly reset, the soonest comes first", () => {
  const list = [claude("a@example.test", 5 * DAY), claude("b@example.test", 3 * HOUR), claude("c@example.test", 2 * DAY + 11 * HOUR)];
  assert.deepEqual(emails(orderAccounts(list, "weekly_reset", now)), ["b@example.test", "c@example.test", "a@example.test"]);
  assert.deepEqual(emails(list), ["a@example.test", "b@example.test", "c@example.test"], "the list itself is left as it was");
});

test("resets within the same minute tie and keep the app's order, so Claude's jitter around the hour moves nothing", () => {
  // Claude's reset times sit on the hour, a fraction of a second either side between readings.
  const early = claude("a@example.test", 2 * DAY + 400);
  const late = claude("b@example.test", 2 * DAY - 32);
  assert.deepEqual(emails(orderAccounts([early, late], "weekly_reset", now)), ["a@example.test", "b@example.test"]);
  assert.deepEqual(emails(orderAccounts([late, early], "weekly_reset", now)), ["b@example.test", "a@example.test"]);
  // A minute apart is a real difference.
  const sooner = claude("c@example.test", 2 * DAY - MINUTE);
  assert.deepEqual(emails(orderAccounts([early, late, sooner], "weekly_reset", now)), ["c@example.test", "a@example.test", "b@example.test"]);
});

test("a week not started counts as a full week from now: after every week under way, tied among themselves", () => {
  const list = [idle("a@example.test"), codex("b@example.test", 6 * DAY + 23 * HOUR), idle("c@example.test"), codex("d@example.test", 2 * DAY)];
  assert.deepEqual(emails(orderAccounts(list, "weekly_reset", now)), ["d@example.test", "b@example.test", "a@example.test", "c@example.test"]);
  assert.equal(weeklyResetAt(list[0], now), now + 7 * DAY);
  // Readings taken a few seconds apart say slightly different times; they still tie.
  const later = { ...idle("e@example.test"), fetched_at: iso(now - 20000) };
  later.windows = [{ ...later.windows[0], resets_at: iso(now - 20000 + 7 * DAY + 1000) }];
  assert.equal(weeklyResetAt(later, now), weeklyResetAt(list[0], now));
});

test("a week that has reset since the reading counts as a full week from now", () => {
  // This computer's last reading failed after the reset passed (stale), or awaits its refresh.
  const passed = claude("a@example.test", -3 * HOUR, 100, { stale: true });
  const running = claude("b@example.test", 6 * DAY + 20 * HOUR, 90);
  assert.equal(weeklyResetAt(passed, now), now + 7 * DAY);
  assert.deepEqual(emails(orderAccounts([passed, running], "weekly_reset", now)), ["b@example.test", "a@example.test"]);
  // A stale reading whose reset is still ahead keeps that time: a week's reset does not move.
  const stale = claude("c@example.test", DAY, 70, { stale: true, error: "token_expired" });
  assert.equal(weeklyResetAt(stale, now), now + DAY);
});

test("another computer's newer reading, the one the flyout shows, decides", () => {
  const a = claude("a@example.test", -2 * DAY, 100, { stale: true });
  a.shared = { machine: "DESK-2", taken_at: iso(now - 10 * MINUTE), windows: [{ kind: "weekly", window_seconds: null, used_percent: 12, resets_at: iso(now + 5 * DAY) }] };
  const b = claude("b@example.test", 6 * DAY);
  assert.equal(weeklyResetAt(a, now), now + 5 * DAY);
  assert.deepEqual(emails(orderAccounts([b, a], "weekly_reset", now)), ["a@example.test", "b@example.test"]);
  // Its week has reset since that computer read it: a fresh week, as for this computer's own.
  a.shared.windows[0].resets_at = iso(now - HOUR);
  assert.equal(weeklyResetAt(a, now), now + 7 * DAY);
});

test("only a weekly or longer limit counts, never the 5-hour one", () => {
  const a = claude("a@example.test", 4 * DAY);
  a.windows.unshift({ kind: "session", window_seconds: null, used_percent: 80, resets_at: iso(now + HOUR) });
  const b = claude("b@example.test", 2 * DAY);
  assert.deepEqual(emails(orderAccounts([a, b], "weekly_reset", now)), ["b@example.test", "a@example.test"]);
  // The overall weekly limit before a model's own, wherever the reading lists it.
  a.windows.unshift({ kind: "weekly_opus", window_seconds: null, used_percent: 30, resets_at: iso(now + HOUR) });
  assert.equal(weeklyResetAt(a, now), now + 4 * DAY);
});

test("no weekly limit: a model's own weekly limit, then the soonest limit of more than a week", () => {
  // A model's own weekly limit before a longer one, the soonest of them.
  const caps = { ...claude("a@example.test", null), windows: [
    { kind: "window", window_seconds: 2592000, used_percent: 10, resets_at: iso(now + DAY) },
    { kind: "weekly_sonnet", window_seconds: null, used_percent: 30, resets_at: iso(now + 3 * DAY) },
    { kind: "weekly_opus", window_seconds: null, used_percent: 30, resets_at: iso(now + 2 * DAY) },
  ] };
  assert.equal(weeklyResetAt(caps, now), now + 2 * DAY);
  // A weekly limit that gives no time gives way to one that does.
  const timeless = claude("b@example.test", null, 35);
  timeless.windows.push({ kind: "weekly_sonnet", window_seconds: null, used_percent: 30, resets_at: iso(now + 2 * DAY) });
  assert.equal(weeklyResetAt(timeless, now), now + 2 * DAY);
  // One that has not started yet still says when it would reset: it stands.
  const fresh = claude("c@example.test", null, 0);
  fresh.windows.push({ kind: "weekly_sonnet", window_seconds: null, used_percent: 30, resets_at: iso(now + 2 * DAY) });
  assert.equal(weeklyResetAt(fresh, now), now + 7 * DAY);
  // A Codex plan's 30 days, under way or not started.
  const monthly = (email, resetsIn, used = 14) => ({ provider: "codex", id: `x-${email}`, email, fetched_at: fetched, stale: false,
    windows: [{ kind: "window", window_seconds: 2592000, used_percent: used, resets_at: iso(resetsIn == null ? Date.parse(fetched) + 30 * DAY + 2000 : now + resetsIn) }] });
  const list = [monthly("a@example.test", 25 * DAY), codex("b@example.test", 6 * DAY), monthly("c@example.test", 2 * DAY), idle("d@example.test"), monthly("e@example.test", null, 0)];
  assert.deepEqual(emails(orderAccounts(list, "weekly_reset", now)), ["c@example.test", "b@example.test", "d@example.test", "a@example.test", "e@example.test"]);
  assert.equal(weeklyResetAt(list[4], now), now + 30 * DAY);
});

test("accounts whose reset is not known come last, in the app's order", () => {
  const none = { provider: "codex", id: "x-a", email: "a@example.test", fetched_at: null, stale: true, error: "token_invalid", windows: [] };
  const short = { provider: "codex", id: "x-b", email: "b@example.test", fetched_at: fetched, stale: false,
    windows: [{ kind: "session", window_seconds: 18000, used_percent: 5, resets_at: iso(now + HOUR) }] };
  const odd = { provider: "codex", id: "x-c", email: "c@example.test", fetched_at: fetched, stale: false,
    windows: [{ kind: "window", window_seconds: null, used_percent: 5, resets_at: iso(now + HOUR) }] };
  const timeless = claude("d@example.test", null, 35);
  const known = claude("e@example.test", 6 * DAY);
  // The weeks nearest the end: not started, and reset since the reading.
  const fresh = idle("f@example.test");
  const passed = claude("g@example.test", -HOUR, 100, { stale: true });
  for (const a of [none, short, odd, timeless]) assert.equal(weeklyResetAt(a, now), null, a.email);
  assert.deepEqual(emails(orderAccounts([none, short, odd, timeless, known, fresh, passed], "weekly_reset", now)),
    ["e@example.test", "f@example.test", "g@example.test", "a@example.test", "b@example.test", "c@example.test", "d@example.test"]);
  // Nothing used and no time said: the week has not begun.
  assert.equal(weeklyResetAt(claude("h@example.test", null, 0), now), now + 7 * DAY);
});

test("the same readings give the same order at every redraw, whatever order they came in", () => {
  // No two of these tie, so the order is the same however they arrive.
  const list = [idle("a@example.test"), claude("b@example.test", DAY), codex("c@example.test", DAY + 2 * MINUTE), claude("d@example.test", null, 50)];
  const first = emails(orderAccounts(list, "weekly_reset", now));
  assert.deepEqual(first, ["b@example.test", "c@example.test", "a@example.test", "d@example.test"]);
  for (let i = 0; i < 20; i++) {
    const shuffled = [...list].sort(() => Math.random() - 0.5);
    assert.deepEqual(emails(orderAccounts(shuffled, "weekly_reset", now + i * 1000)), first);
  }
});

test("both flyout layouts list each provider's accounts in the chosen order", () => {
  // Line endings as git checks the file out on Windows or elsewhere.
  const main = readFileSync(new URL("../src/main.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  const body = (name) => {
    const start = main.indexOf(`function ${name}(`);
    const end = main.indexOf("\n}\n", start);
    assert.ok(start >= 0 && end > start && main.slice(start, end).length < 8000, `${name} found whole, and only it`);
    return main.slice(start, end);
  };
  for (const name of ["renderDetails", "renderCompactList"]) {
    assert.match(body(name), /providerAccounts\(snap, p\)/, name);
    assert.doesNotMatch(body(name), /snap\.accounts\.filter\(/, `${name} lists accounts only through providerAccounts`);
  }
  assert.match(main, /orderAccounts\(snap\.accounts\.filter\(\(a\) => a\.provider === provider\), current\?\.account_order\)/);
});

test("a click just after the rows moved does nothing: it was aimed at the row that was there", () => {
  const main = readFileSync(new URL("../src/main.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  // The rows move only where patch() redraws the list for other keys, and only rows already shown.
  const patch = main.slice(main.indexOf("function patch("), main.indexOf("\n}\n", main.indexOf("function patch(")));
  assert.match(patch, /if \(!sameRows\) \{\n\s*if \(container\.innerHTML !== next\.innerHTML\) \{\n(\s*\/\/.*\n)*\s*if \(container\.querySelector\("\[data-key\]"\)\) rowsMovedAt = performance\.now\(\);/);
  assert.equal(patch.match(/rowsMovedAt =/g).length, 1, "set nowhere else in patch");
  // The list's click handler stops before Use or a monitoring button does anything.
  const handler = main.slice(main.indexOf('$accounts.addEventListener("click"'));
  const firstCode = handler.split("\n").slice(1).find((line) => !/^\s*\/\//.test(line));
  assert.equal(firstCode.trim(), "if (performance.now() - rowsMovedAt < MOVED_CLICK_MS) return;");
  assert.match(main, /let rowsMovedAt = -Infinity;\nconst MOVED_CLICK_MS = 1000;/);
});

console.log(`account order: ${passed} checks passed`);
