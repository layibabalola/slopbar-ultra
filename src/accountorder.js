// How the flyout orders each provider's accounts (Settings > General > Account order, stored as
// `settings::AccountOrder`). They stay grouped by provider either way, and Settings > Accounts
// keeps the app's own order, so a row there never moves while it is being edited.
import { shownReading, notStarted } from "./i18n.js";

const WEEK = 7 * 24 * 3600 * 1000;
const MINUTE = 60000;

/** A limit's length in ms: as the provider says (Codex), else a week for a weekly one (Claude
 *  says none), else 0 (not known). */
const length = (w) => (w.window_seconds > 0 ? w.window_seconds * 1000 : String(w.kind ?? "").startsWith("weekly") ? WEEK : 0);

/** The limits an account's weekly reset is read from, best first: its weekly limit; a model's own
 *  weekly limit (Claude's Opus or Sonnet ones); its limits of more than a week (a Codex plan's
 *  30-day one). A 5-hour limit never counts: it resets several times a day. */
const LIMITS = [(w) => w.kind === "weekly", (w) => w.kind !== "weekly" && length(w) === WEEK, (w) => length(w) > WEEK];

/** When one limit next resets (ms), or null when the reading gives no time. A week under way resets
 *  when the provider says. One not started yet (the time said is a whole window from the reading),
 *  one that has reset since the reading, or one with nothing used and no time said, begins at its
 *  next use and then lasts its full length: that long from now, after every week under way. */
function resetOf(w, readAt, now) {
  const at = w.resets_at ? Date.parse(w.resets_at) : NaN;
  if (Number.isFinite(at) && at > now && !notStarted(w, readAt)) return at;
  if (Number.isFinite(at) || w.used_percent === 0) return now + length(w);
  return null;
}

/** When the account's weekly limit next resets (ms), from the reading the flyout shows (this
 *  computer's, or another's newer one: `shownReading`): the soonest of the best kind of limit
 *  (`LIMITS`) that gives a time, or null when none does. */
export function weeklyResetAt(a, now = Date.now()) {
  const r = shownReading(a, now);
  for (const kind of LIMITS) {
    const times = (r.windows ?? []).filter(kind).map((w) => resetOf(w, r.at, now)).filter((at) => at != null);
    if (times.length) return Math.min(...times);
  }
  return null;
}

/** One provider's accounts in the order chosen. "alphabetical" (and anything else) keeps the app's
 *  order, by email. "weekly_reset" puts the soonest weekly reset first, compared to the minute
 *  (Claude's reset times, on the hour, move by under a second between readings), and accounts with
 *  no reset known last; accounts that tie keep the app's order. */
export function orderAccounts(list, order, now = Date.now()) {
  if (order !== "weekly_reset") return list;
  const keyed = list.map((a) => {
    const at = weeklyResetAt(a, now);
    return [at == null ? Infinity : Math.round(at / MINUTE), a];
  });
  return keyed.sort(([x], [y]) => (x === y ? 0 : x < y ? -1 : 1)).map(([, a]) => a);
}
