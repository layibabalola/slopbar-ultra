//! Plan change advice: concrete moves per provider from the report's verdicts, pool sizing and
//! tiers. Cancel plans that look unused, fold plans that never run out onto one another so their
//! computers share one plan, move underused ones to a cheaper tier that still fits their busiest
//! recent week, and never leave a provider with less than its pool's busiest weeks (90th
//! percentile) plus 20%, or plus 40% while any of its plans keeps running out (so the next advice
//! does not ask for the cut capacity back). Each move says what it saves a month; the provider
//! says how much capacity would be left once every move is made, and which plans could take more.
//!
//! A share ("use X instead of Y, then cancel Y") rests on the accounts, never on which computers
//! use them right now: those change with every switch, and the weeks an account used were made by
//! whichever computers used it then. Both plans must have room to share (`subscriptions::room`:
//! no weekly limit reached in their busiest recent weeks, enough weeks to judge), fresh readings
//! and a known price; together, their busiest weeks plus 20% must fit the plan kept, in every
//! weekly limit (model caps included), and the week in progress may only stop a share, at its use
//! so far or its pace once three days have passed. The 5-hour limit is shared too, which the
//! weekly history cannot check; the move says so.
//!
//! The other way round, a provider whose busiest weeks plus 20% need more than its plans hold, or
//! whose plans keep running out with little to spare, gets the cheapest fix with its monthly cost:
//! upgrade one plan, or add one of the tier that fits. Never in the same advice as a cut.
//!
//! With too little history to size the pool, only clear evidence counts: plans judged unused
//! (weeks watched near zero, or a Codex plan idle for weeks on this PC's history). SlopBar cannot
//! change a plan; every move links to the provider's own billing page.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::accounts::Provider;
use crate::subscriptions::{billing_page, reference_plan, Basis, Confidence, Entry, PlanKind, ProviderSummary, Verdict, POOL_BUFFER};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Cancel,
    Downgrade,
    /// Use another plan instead (`Move::share`), then cancel this one.
    Share,
}

/// What the advice rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The pool has enough weeks: every move keeps the busiest weeks plus 20% covered.
    Pool,
    /// Not enough history to size the pool: only plans judged unused, and no downgrades.
    ClearOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct Move {
    pub provider: Provider,
    pub id: String,
    pub email: Option<String>,
    pub alias: Option<String>,
    pub action: Action,
    /// The plan now, and the tier to move to (downgrades only).
    pub from: String,
    pub to: Option<String>,
    /// What the move saves a month, in dollars.
    pub monthly_saving: f64,
    pub verdict: Verdict,
    pub basis: Basis,
    /// The busiest recent week, in percent of this plan's allowance.
    pub peak_percent: Option<f64>,
    /// The next renewal: the move is worth making before it.
    pub before: Option<DateTime<Utc>>,
    /// Where the plan is changed (fixed per provider).
    pub manage_url: &'static str,
    /// Shares only: the plan to use instead, which is kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<ShareWith>,
}

/// The plan a shared plan's use moves to.
#[derive(Debug, Clone, Serialize)]
pub struct ShareWith {
    pub id: String,
    pub email: Option<String>,
    pub alias: Option<String>,
    /// Its plan once every move is made (a downgrade of it included).
    pub plan: String,
    /// The busiest recent weeks of every plan whose use it would carry, its own included, added
    /// up: in percent of that plan's allowance, for the weekly limit that comes closest (the
    /// overall week or a model cap).
    pub combined_percent: f64,
}

/// A plan that could take more use once every move is made: room to share, and at least
/// `ROOM_MIN` percent of its allowance free beyond the 20% kept spare.
#[derive(Debug, Clone, Serialize)]
pub struct Room {
    pub id: String,
    pub email: Option<String>,
    pub alias: Option<String>,
    pub plan: String,
    /// Its busiest recent weeks, with the plans shared onto it, in percent of its allowance.
    pub busiest_percent: f64,
    /// About how much more of its allowance a week could use, beyond the 20% kept spare.
    pub free_percent: f64,
}

/// A plan with less free than this is not offered as room for more use.
pub const ROOM_MIN: f64 = 10.0;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderAdvice {
    pub provider: Provider,
    pub evidence: Evidence,
    /// The plan capacity is counted in (`subscriptions::reference_plan`).
    pub unit: &'static str,
    /// Weeks of history the pool rests on, and how many it needs.
    pub weeks_used: u8,
    pub weeks_needed: u8,
    /// Paid plans that keep renewing, in `unit`, before and after the moves.
    pub capacity_now: f64,
    pub capacity_after: f64,
    /// The pool's 90th-percentile week, and the least capacity any advice may leave (that + 20%,
    /// or + 40% while any plan keeps running out; this week's use instead when the pool has too
    /// little history).
    pub demand_p90: Option<f64>,
    pub floor: Option<f64>,
    pub moves: Vec<Move>,
    pub monthly_saving: f64,
    /// Capacity left over the busiest weeks after every move, in `unit` and in percent.
    pub spare_after: Option<f64>,
    pub spare_after_percent: Option<f64>,
    /// Unused or underused plans kept because moving them would cut below the floor.
    pub kept_for_capacity: usize,
    /// Too little capacity, and the cheapest fix. Only when there are no moves: a provider is
    /// never told to cancel or downgrade and to add in the same advice.
    pub shortage: Option<Shortage>,
    /// Plans that could take more use once every move is made, the freest first.
    pub room: Vec<Room>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortageCause {
    /// The busiest weeks plus 20% need more than the renewing plans hold.
    OverCapacity,
    /// Plans keep reaching their weekly limit with under 40% to spare over the busiest weeks:
    /// use that hits a limit is cut short, so the real demand is higher than measured.
    HittingLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixAction {
    /// Move one plan to a bigger tier.
    Upgrade,
    /// Buy another plan.
    Add,
}

#[derive(Debug, Clone, Serialize)]
pub struct Shortage {
    pub cause: ShortageCause,
    /// Capacity missing, in `unit`.
    pub shortfall: f64,
    /// Plans judged to run out most weeks.
    pub exhausted: usize,
    pub fix: Fix,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fix {
    pub action: FixAction,
    /// Upgrade: the account and its plan now.
    pub id: Option<String>,
    pub email: Option<String>,
    pub alias: Option<String>,
    pub from: Option<String>,
    pub to: String,
    /// Plans of `to` to add: one, unless no single tier covers the shortfall.
    pub count: u32,
    /// What the fix adds to the monthly bill, in dollars (list prices).
    pub monthly_cost: f64,
    pub capacity_after: f64,
    pub manage_url: &'static str,
}

/// Tiers a provider sells, largest first: (name, weekly allowance in `reference_plan`s, US list
/// price per month). Same weights as `subscriptions::plan_weight`.
fn tiers(provider: Provider) -> &'static [(&'static str, f64, f64)] {
    match provider {
        Provider::Claude => &[("Max 20x", 1.0, 200.0), ("Max 5x", 0.25, 100.0), ("Pro", 0.05, 20.0)],
        Provider::Codex => &[("Pro 500", 2.5, 500.0), ("Pro 200", 1.0, 200.0), ("Pro 100", 0.5, 100.0), ("Plus", 0.1, 20.0)],
    }
}

/// A plan the advisor may move: paid by the user, still renewing, of known size and cost.
struct Candidate<'a> {
    entry: &'a Entry,
    plan: String,
    weight: f64,
    cost: f64,
}

pub fn advise(accounts: &[Entry], providers: &[ProviderSummary]) -> Vec<ProviderAdvice> {
    providers.iter().filter(|s| s.paid > 0).map(|s| advise_provider(s, accounts)).collect()
}

fn advise_provider(summary: &ProviderSummary, accounts: &[Entry]) -> ProviderAdvice {
    let provider = summary.provider;
    let pool = &summary.pool;
    // Plans already ending or being let lapse are capacity that is going away anyway.
    let renewing: Vec<&Entry> = accounts.iter()
        .filter(|e| e.provider == provider && e.plan_kind == PlanKind::Personal && e.ends_at.is_none() && e.cancel_after.is_none())
        .collect();
    let capacity_now: f64 = renewing.iter().filter_map(|e| e.plan_weight).sum();
    let evidence = if pool.demand_p90.is_some() { Evidence::Pool } else { Evidence::ClearOnly };
    // While plans keep running out, their use is cut short and needs more than was measured: every
    // cut then keeps the spare `shortage` asks of the plans held, so the next advice does not ask
    // for the cut capacity back.
    let spare = if summary.exhausted > 0 { 2.0 * POOL_BUFFER } else { POOL_BUFFER };
    let floor = match evidence {
        Evidence::Pool => pool.demand_p90.map(|d| d * (1.0 + spare)),
        Evidence::ClearOnly => pool.live_demand.map(|d| d * (1.0 + spare)),
    };
    let mut candidates: Vec<Candidate> = renewing.iter().filter_map(|e| Some(Candidate {
        entry: e, plan: e.plan.clone()?, weight: e.plan_weight?, cost: e.monthly_cost?,
    })).collect();
    // The biggest savings first, then a stable order.
    candidates.sort_by(|a, b| b.cost.total_cmp(&a.cost).then_with(|| a.entry.id.cmp(&b.entry.id)));

    let mut capacity = capacity_now;
    let mut plans_left = renewing.len();
    let mut moves = Vec::new();
    let mut kept = 0;
    let fits = |after: f64| floor.is_none_or(|f| after + 1e-9 >= f);
    let movable = |c: &Candidate| match evidence {
        Evidence::Pool => matches!(c.entry.assessment.verdict, Verdict::Unused | Verdict::Underused),
        Evidence::ClearOnly => c.entry.assessment.verdict == Verdict::Unused,
    };
    // Cancels first: an unused plan frees its whole price.
    for c in candidates.iter().filter(|c| movable(c) && c.entry.assessment.verdict == Verdict::Unused) {
        // Never advise dropping a provider altogether.
        if plans_left <= 1 || !fits(capacity - c.weight) { kept += 1; continue; }
        capacity -= c.weight;
        plans_left -= 1;
        moves.push(new_move(c, Action::Cancel, None, c.cost));
    }
    // Then shares: a plan whose use fits on another frees its whole price too. Only on the pool's
    // evidence, as downgrades: a share leaves less spare than either plan had.
    let sharers: Vec<Sharer> = match evidence {
        Evidence::Pool => {
            let mut s: Vec<Sharer> = candidates.iter().filter(|c| !moves.iter().any(|m: &Move| m.id == c.entry.id) && can_share(c)).map(Sharer::of).collect();
            s.sort_by(|a, b| b.busiest.total_cmp(&a.busiest).then_with(|| a.c.entry.id.cmp(&b.c.entry.id)));
            s
        }
        Evidence::ClearOnly => vec![],
    };
    let groups = plan_shares(&Search { provider, sharers: &sharers, capacity, fits: &fits });
    capacity -= groups.iter().flat_map(|g| &g.guests).map(|&i| sharers[i].c.weight).sum::<f64>();
    let shared_away = |id: &str| groups.iter().any(|g| g.guests.iter().any(|&i| sharers[i].c.entry.id == id));
    let hosted = |id: &str| groups.iter().find(|g| sharers[g.host].c.entry.id == id);
    // Then underused plans move to the cheapest smaller tier that still fits their busiest week:
    // a plan kept for others, with their use added.
    let mut downgrades = Vec::new();
    let mut tier_after: Vec<(&str, &'static str, f64)> = Vec::new();
    if evidence == Evidence::Pool {
        for c in candidates.iter().filter(|c| c.entry.assessment.verdict == Verdict::Underused && !shared_away(&c.entry.id)) {
            let need = match hosted(&c.entry.id) {
                Some(g) => combined(&sharers, &g.members()) * (1.0 + POOL_BUFFER),
                None => { let Some(peak) = c.entry.recent_peak() else { kept += 1; continue }; peak / 100.0 * c.weight * (1.0 + POOL_BUFFER) }
            };
            let target = tiers(provider).iter().rev()
                .filter(|(_, w, price)| *w < c.weight && *price < c.cost)
                .find(|(_, w, _)| *w + 1e-9 >= need && fits(capacity - (c.weight - w)));
            match target {
                Some((name, w, price)) => {
                    capacity -= c.weight - w;
                    tier_after.push((c.entry.id.as_str(), name, *w));
                    downgrades.push(new_move(c, Action::Downgrade, Some(name.to_string()), c.cost - price));
                }
                None => kept += 1,
            }
        }
    }
    // Each plan's tier once every move is made.
    let after = |s: &Sharer| tier_after.iter().find(|(id, ..)| *id == s.c.entry.id).map_or((s.c.plan.clone(), s.c.weight), |(_, name, w)| (name.to_string(), *w));
    let mut shared = Vec::new();
    for g in &groups {
        let h = &sharers[g.host];
        let (plan, weight) = after(h);
        let combined_percent = round(combined(&sharers, &g.members()) / weight * 100.0);
        for &i in &g.guests {
            let mut m = new_move(sharers[i].c, Action::Share, None, sharers[i].c.cost);
            m.peak_percent = sharers[i].c.entry.share.load().iter().find(|(k, _)| k == "weekly").map(|(_, p)| round(*p));
            let e = h.c.entry;
            m.share = Some(ShareWith { id: e.id.clone(), email: e.email.clone(), alias: e.alias.clone(), plan: plan.clone(), combined_percent });
            shared.push(m);
        }
    }
    // The biggest savings first, as the cancels.
    shared.sort_by(|a, b| b.monthly_saving.total_cmp(&a.monthly_saving).then_with(|| a.id.cmp(&b.id)));
    moves.extend(shared);
    moves.extend(downgrades);
    // What could take more use once every move is made.
    let mut room: Vec<Room> = sharers.iter().enumerate().filter(|(_, s)| !shared_away(&s.c.entry.id)).filter_map(|(i, s)| {
        let members = groups.iter().find(|g| g.host == i).map_or(vec![i], |g| g.members());
        let (plan, weight) = after(s);
        let busiest = combined(&sharers, &members) / weight * 100.0;
        let free = 100.0 / (1.0 + POOL_BUFFER) - busiest;
        let e = s.c.entry;
        (free + 1e-9 >= ROOM_MIN).then(|| Room { id: e.id.clone(), email: e.email.clone(), alias: e.alias.clone(), plan, busiest_percent: round(busiest), free_percent: round(free) })
    }).collect();
    room.sort_by(|a, b| b.free_percent.total_cmp(&a.free_percent).then_with(|| a.id.cmp(&b.id)));
    let demand = pool.demand_p90.or(match evidence { Evidence::ClearOnly => pool.live_demand, Evidence::Pool => None });
    let shortage = if moves.is_empty() { shortage(provider, &candidates, capacity_now, demand, summary.exhausted) } else { None };
    ProviderAdvice {
        provider,
        evidence,
        unit: reference_plan(provider),
        weeks_used: pool.weeks_used,
        weeks_needed: pool.weeks_needed,
        capacity_now: round(capacity_now),
        capacity_after: round(capacity),
        demand_p90: pool.demand_p90,
        floor: floor.map(round),
        monthly_saving: round(moves.iter().map(|m: &Move| m.monthly_saving).sum()),
        spare_after: demand.map(|d| round(capacity - d)),
        spare_after_percent: demand.filter(|d| *d > 0.0).map(|d| round((capacity / d - 1.0) * 100.0)),
        moves,
        kept_for_capacity: kept,
        shortage,
        room,
    }
}

/// A plan that may take part in a share (`can_share`), with the use it brings.
struct Sharer<'a> {
    c: &'a Candidate<'a>,
    /// Its use to plan for per weekly limit (`ShareBasis::load`), in plans of `reference_plan`.
    load: Vec<(String, f64)>,
    /// Its busiest completed week overall, in plans: on a tie the busier plan is the one kept, so
    /// the lighter use is what moves.
    busiest: f64,
}

impl<'a> Sharer<'a> {
    fn of(c: &'a Candidate<'a>) -> Sharer<'a> {
        let s = &c.entry.share;
        Sharer {
            c,
            load: s.load().into_iter().map(|(k, p)| (k, p / 100.0 * c.weight)).collect(),
            busiest: s.busiest.iter().find(|(k, _)| k == "weekly").map_or(0.0, |(_, p)| p / 100.0 * c.weight),
        }
    }
}

/// Whether a plan may take part in a share: in use without running out (healthy or underused; an
/// unused one is cancelled instead), room to share (`subscriptions::room`: no weekly limit reached
/// in its busiest recent weeks, over enough weeks to judge) and a fresh reading.
fn can_share(c: &Candidate) -> bool {
    let e = c.entry;
    matches!(e.assessment.verdict, Verdict::Healthy | Verdict::Underused) && e.assessment.confidence == Confidence::High
        && e.share.room && e.share.fresh
}

/// Plans kept as one: `host` keeps its plan and carries the use of `guests`, which are cancelled.
/// Indices into the sharers.
struct Group {
    host: usize,
    guests: Vec<usize>,
}

impl Group {
    fn members(&self) -> Vec<usize> {
        std::iter::once(self.host).chain(self.guests.iter().copied()).collect()
    }
}

/// The use of `members` added up, in plans, in the weekly limit where it comes to most: each
/// weekly limit, model caps included, is the same share of a plan's allowance.
fn combined(sharers: &[Sharer], members: &[usize]) -> f64 {
    let mut kinds: Vec<&str> = vec![];
    for &m in members {
        for (k, _) in &sharers[m].load {
            if !kinds.contains(&k.as_str()) { kinds.push(k); }
        }
    }
    kinds.iter()
        .map(|k| members.iter().map(|&m| sharers[m].load.iter().find(|(x, _)| x == k).map_or(0.0, |(_, v)| *v)).sum::<f64>())
        .fold(0.0, f64::max)
}

/// The plan to keep for `members`, when one can carry them all: its allowance covers their use
/// added up plus 20% in every weekly limit, every other member's use is the user's own (use from
/// outside the user's computers would not move with them), and for Codex all are one tier (its
/// tiers' sizes are price guesses). The cheapest such plan, then the one renewing last (cancelling
/// the others saves sooner), then the busiest, then by id.
fn host(sharers: &[Sharer], members: &[usize], provider: Provider) -> Option<usize> {
    if let [only] = members { return Some(*only); }
    if provider == Provider::Codex && members.iter().any(|&m| sharers[m].c.plan != sharers[members[0]].c.plan) { return None; }
    let need = combined(sharers, members) * (1.0 + POOL_BUFFER);
    let outside: Vec<usize> = members.iter().copied().filter(|&m| sharers[m].c.entry.share.outside).collect();
    let renews = |s: &Sharer| s.c.entry.renewal.as_ref().map_or(DateTime::<Utc>::MAX_UTC, |r| r.at);
    members.iter().copied()
        .filter(|&h| sharers[h].c.weight + 1e-9 >= need && outside.iter().all(|&o| o == h))
        .min_by(|&a, &b| {
            let (x, y) = (&sharers[a], &sharers[b]);
            x.c.cost.total_cmp(&y.c.cost)
                .then_with(|| renews(y).cmp(&renews(x)))
                .then_with(|| y.busiest.total_cmp(&x.busiest))
                .then_with(|| x.c.entry.id.cmp(&y.c.entry.id))
        })
}

/// What a share search works with.
struct Search<'a, 'b> {
    provider: Provider,
    sharers: &'b [Sharer<'a>],
    /// Capacity after the cancels, and whether what a grouping leaves is enough.
    capacity: f64,
    fits: &'b dyn Fn(f64) -> bool,
}

/// Every grouping is tried up to this many plans; beyond it, plans are placed one at a time.
const SEARCH_MAX: usize = 10;

/// The shares that save the most: groups of plans, each kept on one of them (`host`), within what
/// the provider must keep (`Search::fits`); on a tie, the fewest plans to move. Every grouping is
/// tried when there are at most `SEARCH_MAX` plans that may share (placing them one by one can
/// strand a plan as a host that a better grouping cancels); beyond that, each plan from the
/// dearest, still alone, joins the group where the whole saves most.
fn plan_shares(s: &Search) -> Vec<Group> {
    let n = s.sharers.len();
    if n < 2 { return vec![]; }
    let parts = if n <= SEARCH_MAX {
        let mut best = None;
        search(s, 0, &mut vec![], &mut best);
        best.map(|(_, _, parts)| parts).unwrap_or_default()
    } else {
        greedy(s)
    };
    parts.into_iter().filter(|p| p.len() > 1)
        .filter_map(|p| { let h = host(s.sharers, &p, s.provider)?; Some(Group { host: h, guests: p.into_iter().filter(|&m| m != h).collect() }) })
        .collect()
}

/// A grouping's monthly saving, the plans it cancels and their capacity, when every group has a
/// plan to keep.
fn evaluate(s: &Search, parts: &[Vec<usize>]) -> Option<(f64, usize, f64)> {
    let (mut saving, mut guests, mut freed) = (0.0, 0, 0.0);
    for p in parts.iter().filter(|p| p.len() > 1) {
        let h = host(s.sharers, p, s.provider)?;
        for &m in p.iter().filter(|&&m| m != h) {
            saving += s.sharers[m].c.cost;
            freed += s.sharers[m].c.weight;
            guests += 1;
        }
    }
    Some((saving, guests, freed))
}

type Best = Option<(f64, usize, Vec<Vec<usize>>)>;

/// Tries every way to group the sharers from `i` on, given `parts` so far, keeping the best. A
/// group that no plan of it can carry is never grown: more members only add use.
fn search(s: &Search, i: usize, parts: &mut Vec<Vec<usize>>, best: &mut Best) {
    if i == s.sharers.len() {
        let Some((saving, guests, freed)) = evaluate(s, parts) else { return };
        if !(s.fits)(s.capacity - freed) { return; }
        if best.as_ref().is_none_or(|(b, g, _)| saving > b + 1e-9 || saving > b - 1e-9 && guests < *g) {
            *best = Some((saving, guests, parts.clone()));
        }
        return;
    }
    for g in 0..parts.len() {
        parts[g].push(i);
        if host(s.sharers, &parts[g], s.provider).is_some() { search(s, i + 1, parts, best); }
        parts[g].pop();
    }
    parts.push(vec![i]);
    search(s, i + 1, parts, best);
    parts.pop();
}

/// Beyond `SEARCH_MAX`: each plan still alone, the dearest first (then the lightest), joins the
/// group where the whole grouping saves most, while what it leaves is enough.
fn greedy(s: &Search) -> Vec<Vec<usize>> {
    let n = s.sharers.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&s.sharers[a], &s.sharers[b]);
        y.c.cost.total_cmp(&x.c.cost).then_with(|| x.busiest.total_cmp(&y.busiest)).then_with(|| x.c.entry.id.cmp(&y.c.entry.id))
    });
    let mut parts: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    for c in order {
        let Some(from) = parts.iter().position(|p| p.as_slice() == [c]) else { continue };
        let now = evaluate(s, &parts).map_or(0.0, |(saving, ..)| saving);
        let mut best: Option<(f64, usize)> = None;
        for t in (0..parts.len()).filter(|&t| t != from) {
            let mut trial = parts.clone();
            trial[t].push(c);
            trial.remove(from);
            let Some((saving, _, freed)) = evaluate(s, &trial) else { continue };
            if (s.fits)(s.capacity - freed) && saving > now + 1e-9 && best.is_none_or(|(b, _)| saving > b + 1e-9) {
                best = Some((saving, t));
            }
        }
        if let Some((_, t)) = best {
            parts[t].push(c);
            parts.remove(from);
        }
    }
    parts
}

/// Whether the plans fall short of the demand, and the cheapest way to cover it: upgrading one
/// plan to a bigger tier, or adding one plan of the smallest tier that covers the shortfall (or
/// several of the largest, when none does). Ties go to the fix that adds more capacity.
fn shortage(provider: Provider, candidates: &[Candidate], capacity: f64, demand: Option<f64>, exhausted: usize) -> Option<Shortage> {
    let demand = demand?;
    let over = demand * (1.0 + POOL_BUFFER) - capacity;
    let limited = demand * (1.0 + 2.0 * POOL_BUFFER) - capacity;
    let (cause, need) = if over > 1e-9 { (ShortageCause::OverCapacity, over) }
        else if exhausted > 0 && limited > 1e-9 { (ShortageCause::HittingLimits, limited) }
        else { return None };
    let tiers = tiers(provider);
    let manage_url = billing_page(provider);
    let mut options: Vec<Fix> = vec![];
    for c in candidates {
        for (name, w, price) in tiers.iter().filter(|(_, w, _)| *w > c.weight && *w - c.weight + 1e-9 >= need) {
            let e = c.entry;
            options.push(Fix { action: FixAction::Upgrade, id: Some(e.id.clone()), email: e.email.clone(), alias: e.alias.clone(), from: Some(c.plan.clone()),
                to: name.to_string(), count: 1, monthly_cost: round((price - c.cost).max(0.0)), capacity_after: round(capacity + w - c.weight), manage_url });
        }
    }
    let add = |(name, w, price): &(&str, f64, f64), count: u32| Fix { action: FixAction::Add, id: None, email: None, alias: None, from: None,
        to: name.to_string(), count, monthly_cost: round(price * count as f64), capacity_after: round(capacity + w * count as f64), manage_url };
    match tiers.iter().filter(|(_, w, _)| *w + 1e-9 >= need).min_by(|a, b| a.2.total_cmp(&b.2)) {
        Some(t) => options.push(add(t, 1)),
        None => options.push(add(&tiers[0], (need / tiers[0].1 - 1e-9).ceil() as u32)),
    }
    let fix = options.into_iter().min_by(|a, b| a.monthly_cost.total_cmp(&b.monthly_cost).then_with(|| b.capacity_after.total_cmp(&a.capacity_after)))?;
    Some(Shortage { cause, shortfall: round(need), exhausted, fix })
}

fn new_move(c: &Candidate, action: Action, to: Option<String>, saving: f64) -> Move {
    let e = c.entry;
    Move {
        provider: e.provider, id: e.id.clone(), email: e.email.clone(), alias: e.alias.clone(), action,
        from: c.plan.clone(), to, monthly_saving: round(saving), verdict: e.assessment.verdict, basis: e.assessment.basis,
        peak_percent: e.recent_peak(), before: e.renewal.as_ref().map(|r| r.at), manage_url: billing_page(e.provider), share: None,
    }
}

fn round(v: f64) -> f64 { (v * 100.0).round() / 100.0 }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountUsage, Snapshot};
    use crate::plan_alerts::tests::{at, watched};
    use crate::settings::Settings;
    use crate::subscriptions::{report, AccountHistory, History, WindowRecord};
    use chrono::Duration;

    fn plan(provider: &str, id: &str, plan: &str, tier: Option<&str>, used: f64, now: DateTime<Utc>) -> AccountUsage {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider": provider, "email": format!("{id}@example.invalid"), "alias": null, "plan": plan, "org": null,
            "active": false, "sources": [], "credits": null, "resets": [], "error": null, "stale": false, "fetched_at": now,
            "windows": [{ "kind": "weekly", "window_seconds": if provider == "codex" { Some(604800) } else { None }, "used_percent": used, "resets_at": now + Duration::days(3) }],
            "monitor_connected": true, "billing": { "tier": tier } })).unwrap()
    }

    /// Claude Max plans, each with four watched weeks peaking at the given share.
    fn claude(fleet: &[(&str, &str, f64)], now: DateTime<Utc>) -> Vec<ProviderAdvice> {
        let mut history = History::default();
        let accounts = fleet.iter().map(|(id, tier, peak)| {
            history.accounts.insert(format!("Claude:{id}"), watched(*peak, now));
            plan("claude", id, "max", Some(&format!("default_claude_max_{tier}")), 1.0, now)
        }).collect();
        report(&Snapshot { accounts, updated_at: Some(now) }, &history, &Settings::default(), None, now).advice
    }

    #[test]
    fn unused_plans_are_cancelled_and_underused_ones_downgraded_while_the_busiest_weeks_fit() {
        let now = at("2026-10-21T12:00:00Z");
        // Busy at 75%: the light plan's use does not fit on it ((0.75 + 0.15) x 1.2 > 1), so it is downgraded.
        let a = &claude(&[("busy", "20x", 75.0), ("idle1", "20x", 0.0), ("idle2", "20x", 2.0), ("light", "20x", 15.0)], now)[0];
        assert_eq!((a.evidence, a.unit, a.capacity_now), (Evidence::Pool, "Max 20x", 4.0));
        // Busiest week 0.75 + 0.15 + 0.02 = 0.92 plans; floor 1.104.
        assert!((a.demand_p90.unwrap() - 0.92).abs() < 1e-9 && (a.floor.unwrap() - 1.1).abs() < 1e-9);
        let moves: Vec<(&str, Action, Option<&str>, f64)> = a.moves.iter().map(|m| (m.id.as_str(), m.action, m.to.as_deref(), m.monthly_saving)).collect();
        assert_eq!(moves, [("idle1", Action::Cancel, None, 200.0), ("idle2", Action::Cancel, None, 200.0), ("light", Action::Downgrade, Some("Max 5x"), 100.0)]);
        assert_eq!((a.monthly_saving, a.capacity_after, a.kept_for_capacity), (500.0, 1.25, 0));
        assert_eq!((a.spare_after, a.spare_after_percent), (Some(0.33), Some(35.87)));
        let m = &a.moves[0];
        assert_eq!((m.email.as_deref(), m.manage_url, m.verdict), (Some("idle1@example.invalid"), "https://claude.ai/settings/billing", Verdict::Unused));
        // At 60% the light plan's use fits on the busy one: sharing frees its whole price, more than the downgrade.
        let a = &claude(&[("busy", "20x", 60.0), ("idle1", "20x", 0.0), ("idle2", "20x", 2.0), ("light", "20x", 15.0)], now)[0];
        let moves: Vec<(&str, Action, f64)> = a.moves.iter().map(|m| (m.id.as_str(), m.action, m.monthly_saving)).collect();
        assert_eq!(moves, [("idle1", Action::Cancel, 200.0), ("idle2", Action::Cancel, 200.0), ("light", Action::Share, 200.0)]);
        assert_eq!((a.monthly_saving, a.capacity_after, a.spare_after_percent), (600.0, 1.0, Some(29.87)));
        let s = a.moves[2].share.as_ref().unwrap();
        assert_eq!((s.id.as_str(), s.plan.as_str(), s.combined_percent), ("busy", "Max 20x", 75.0));
    }

    /// A history of completed weeks, oldest first, each watched for most of its hours.
    fn weeks_of(kind: &str, peaks: &[f64], now: DateTime<Utc>) -> Vec<WindowRecord> {
        let n = peaks.len() as i64;
        peaks.iter().enumerate().map(|(i, &peak)| {
            let end = now - Duration::days(7 * (n - i as i64) - 6);
            WindowRecord { kind: kind.into(), resets_at: end, peak, first_obs: end - Duration::days(7), last_obs: end - Duration::hours(1),
                hours: 120, days: 7, idle: false, local: false }
        }).collect()
    }

    fn history_of(peaks: &[f64], now: DateTime<Utc>) -> AccountHistory {
        AccountHistory { first_seen: Some(now - Duration::days(7 * peaks.len() as i64 + 9)), windows: weeks_of("weekly", peaks, now), ..Default::default() }
    }

    /// Plans of one provider, each with its own weeks (oldest first), as that provider's advice.
    fn advice_for(provider: &str, fleet: Vec<(AccountUsage, AccountHistory)>, now: DateTime<Utc>) -> ProviderAdvice {
        let mut history = History::default();
        let name = if provider == "claude" { "Claude" } else { "Codex" };
        let accounts = fleet.into_iter().map(|(acc, h)| { history.accounts.insert(format!("{name}:{}", acc.id), h); acc }).collect();
        report(&Snapshot { accounts, updated_at: Some(now) }, &history, &Settings::default(), None, now).advice.remove(0)
    }

    /// Claude Max plans ("20x" or "5x") with the given weeks.
    fn claude_weeks(fleet: &[(&str, &str, &[f64])], now: DateTime<Utc>) -> ProviderAdvice {
        advice_for("claude", fleet.iter().map(|(id, tier, peaks)| (plan("claude", id, "max", Some(&format!("default_claude_max_{tier}")), 1.0, now), history_of(peaks, now))).collect(), now)
    }

    fn shares(a: &ProviderAdvice) -> Vec<(&str, &str, f64)> {
        a.moves.iter().filter(|m| m.action == Action::Share).map(|m| (m.id.as_str(), m.share.as_ref().unwrap().id.as_str(), m.monthly_saving)).collect()
    }

    #[test]
    fn two_light_plans_share_one_and_the_other_is_cancelled() {
        let now = at("2026-11-25T12:00:00Z");
        let a = claude_weeks(&[("a", "20x", &[30.0; 4]), ("b", "20x", &[20.0; 4])], now);
        // Together 0.5 plans, plus 20% is 0.6 of one: the busier plan is kept, the lighter one's use moves.
        assert_eq!(shares(&a), [("b", "a", 200.0)]);
        assert_eq!(a.moves.len(), 1, "the plan shared away is not also downgraded");
        let m = &a.moves[0];
        assert_eq!((m.from.as_str(), m.peak_percent, m.before, m.verdict), ("Max 20x", Some(20.0), None, Verdict::Underused));
        assert_eq!(m.share.as_ref().map(|s| (s.email.as_deref(), s.plan.as_str(), s.combined_percent)), Some((Some("a@example.invalid"), "Max 20x", 50.0)));
        assert_eq!((a.monthly_saving, a.capacity_after), (200.0, 1.0));
        // What is kept could take a little more: 83.33% of it may be used before the 20% spare.
        assert_eq!(a.room.iter().map(|r| (r.id.as_str(), r.busiest_percent, r.free_percent)).collect::<Vec<_>>(), [("a", 50.0, 33.33)]);
        // Only a share's move carries the plan to use; the others serialize as before.
        let json = serde_json::to_value(&a.moves[0]).unwrap();
        assert_eq!((json["action"].as_str(), json["share"]["id"].as_str()), (Some("share"), Some("a")));
        let a = claude_weeks(&[("busy", "20x", &[75.0; 4]), ("light", "20x", &[15.0; 4])], now);
        assert!(serde_json::to_value(&a.moves[0]).unwrap().get("share").is_none());
    }

    #[test]
    fn a_plan_kept_for_another_is_downgraded_only_to_a_tier_that_carries_both() {
        let now = at("2026-11-25T12:00:00Z");
        // 10% and 10%: one plan carries both at 0.2, and Max 5x (0.25) still covers that plus 20%.
        let a = claude_weeks(&[("a", "20x", &[10.0; 4]), ("b", "20x", &[10.0; 4])], now);
        let moves: Vec<(&str, Action, Option<&str>, f64)> = a.moves.iter().map(|m| (m.id.as_str(), m.action, m.to.as_deref(), m.monthly_saving)).collect();
        assert_eq!(moves, [("b", Action::Share, None, 200.0), ("a", Action::Downgrade, Some("Max 5x"), 100.0)]);
        // The share says what the kept plan will be, and how full.
        assert_eq!(a.moves[0].share.as_ref().map(|s| (s.plan.as_str(), s.combined_percent)), Some(("Max 5x", 80.0)));
        assert!(a.room.is_empty(), "a plan at 80% has no room beyond its spare");
        // At 15% each, 0.3 plus 20% needs more than Max 5x: the kept plan stays as it is.
        let a = claude_weeks(&[("a", "20x", &[15.0; 4]), ("b", "20x", &[15.0; 4])], now);
        assert_eq!(a.moves.iter().map(|m| m.action).collect::<Vec<_>>(), [Action::Share]);
    }

    #[test]
    fn every_grouping_is_tried_so_no_plan_is_stranded_as_a_host() {
        let now = at("2026-11-25T12:00:00Z");
        // Placing the busiest first would keep the Max 5x at 80% for itself and free one Max 20x ($200);
        // keeping one Max 20x for all three frees both others ($300).
        let a = claude_weeks(&[("a", "5x", &[80.0; 4]), ("b", "20x", &[15.0; 4]), ("c", "20x", &[10.0; 4])], now);
        assert_eq!(shares(&a), [("c", "b", 200.0), ("a", "b", 100.0)]);
        assert_eq!(a.moves[0].share.as_ref().unwrap().combined_percent, 45.0);
        // 45% + 96% of a Max 5x + 22%: only one Max 20x can go, and the dearer plan is the one freed.
        let a = claude_weeks(&[("a", "20x", &[45.0; 4]), ("b", "5x", &[96.0; 4]), ("c", "20x", &[22.0; 4])], now);
        assert_eq!(shares(&a), [("c", "a", 200.0)]);
        // A cheaper plan that can carry both is the one kept.
        let a = claude_weeks(&[("big", "20x", &[5.0, 6.0, 8.0, 6.0]), ("small", "5x", &[40.0; 4])], now);
        assert_eq!(shares(&a), [("big", "small", 200.0)]);
        // A smaller plan's use fits on a bigger one: Max 5x at 40% is 0.1 of a Max 20x.
        let a = claude_weeks(&[("a", "20x", &[30.0; 4]), ("b", "5x", &[40.0; 4])], now);
        assert_eq!(shares(&a), [("b", "a", 100.0)]);
    }

    #[test]
    fn a_share_fits_every_weekly_limit_model_caps_included() {
        let now = at("2026-11-25T12:00:00Z");
        let capped = |id: &str, weekly: f64, opus: f64| {
            let mut h = history_of(&[weekly; 4], now);
            h.windows.extend(weeks_of("weekly_opus", &[opus; 4], now));
            (plan("claude", id, "max", Some("default_claude_max_20x"), 1.0, now), h)
        };
        // Overall 0.4 + 0.3 fits, but Opus 0.7 + 0.6 does not.
        assert!(shares(&advice_for("claude", vec![capped("a", 40.0, 70.0), capped("b", 30.0, 60.0)], now)).is_empty());
        assert_eq!(shares(&advice_for("claude", vec![capped("a", 40.0, 30.0), capped("b", 30.0, 20.0)], now)), [("b", "a", 200.0)]);
        // A model cap that ran out in one of the latest eight weeks leaves no room to share.
        assert!(shares(&advice_for("claude", vec![capped("a", 40.0, 99.6), capped("b", 30.0, 20.0)], now)).is_empty());
    }

    #[test]
    fn only_plans_that_never_ran_out_over_enough_weeks_share() {
        let now = at("2026-11-25T12:00:00Z");
        // A full week behind four quiet ones still counts among the latest eight.
        let a = claude_weeks(&[("a", "20x", &[100.0, 30.0, 30.0, 30.0, 30.0]), ("b", "20x", &[20.0; 4])], now);
        assert_eq!(a.moves.iter().filter(|m| m.action == Action::Share).count(), 0);
        // Three weeks are too few to share on: two downgrades instead.
        let a = claude_weeks(&[("a", "20x", &[20.0; 3]), ("b", "20x", &[20.0; 3])], now);
        assert_eq!(a.moves.iter().map(|m| (m.id.as_str(), m.action)).collect::<Vec<_>>(), [("a", Action::Downgrade), ("b", Action::Downgrade)]);
        // One week: only clear evidence counts, and nothing is shared.
        let a = claude_weeks(&[("a", "20x", &[20.0]), ("b", "20x", &[20.0])], now);
        assert_eq!((a.evidence, a.moves.len(), a.room.len()), (Evidence::ClearOnly, 0, 0));
    }

    #[test]
    fn the_week_in_progress_can_stop_a_share() {
        let now = at("2026-11-25T12:00:00Z");
        let with_week = |id: &str, peaks: &[f64], used: f64, resets_in: Duration| {
            let mut acc = plan("claude", id, "max", Some("default_claude_max_20x"), used, now);
            acc.windows[0].resets_at = Some(now + resets_in);
            (acc, history_of(peaks, now))
        };
        // Four days in at 30%, a pace of 52.5%: with the other plan's 35%, 0.875 plus 20% no longer fits.
        let busy = advice_for("claude", vec![with_week("a", &[30.0; 4], 30.0, Duration::days(3)), with_week("b", &[35.0; 4], 1.0, Duration::days(3))], now);
        assert!(shares(&busy).is_empty());
        // The same 30% a day in is a burst, not a pace: the share stands, onto the plan with the busier weeks.
        let burst = advice_for("claude", vec![with_week("a", &[30.0; 4], 30.0, Duration::days(6)), with_week("b", &[35.0; 4], 1.0, Duration::days(3))], now);
        assert_eq!(shares(&burst), [("a", "b", 200.0)]);
        // A plan without a fresh reading is never part of a share.
        let (mut stale, h) = with_week("b", &[35.0; 4], 1.0, Duration::days(3));
        stale.stale = true;
        assert!(shares(&advice_for("claude", vec![with_week("a", &[30.0; 4], 1.0, Duration::days(3)), (stale, h)], now)).is_empty());
    }

    #[test]
    fn codex_shares_within_one_tier_and_never_cancel_use_from_outside() {
        // Four weeks begun after Pro 200's allowance cut (2026-10-29): they count as read.
        let now = at("2026-12-02T12:00:00Z");
        let codex = |id: &str, plan_type: &str, peak: f64| (plan("codex", id, plan_type, None, 1.0, now), history_of(&[peak; 4], now));
        assert_eq!(shares(&advice_for("codex", vec![codex("a", "pro", 30.0), codex("b", "pro", 20.0)], now)), [("b", "a", 200.0)]);
        // A Plus plan's size next to Pro is a price guess: no share across tiers.
        assert!(shares(&advice_for("codex", vec![codex("a", "pro", 30.0), codex("b", "plus", 20.0)], now)).is_empty());
        // Use from where none of the user's computers had it signed in would not move with them: that plan
        // is the one kept, or none is.
        let (mut outside, h) = codex("b", "pro", 20.0);
        outside.used_outside = true;
        assert_eq!(shares(&advice_for("codex", vec![codex("a", "pro", 30.0), (outside.clone(), h.clone())], now)), [("a", "b", 200.0)]);
        let (mut other, h2) = codex("a", "pro", 30.0);
        other.used_outside = true;
        assert!(shares(&advice_for("codex", vec![(other, h2), (outside, h)], now)).is_empty());
    }

    #[test]
    fn pro_200_weeks_before_its_allowance_cut_count_double_in_a_share() {
        // Four weeks to 2026-11-24: the oldest began 2026-10-27, under the old allowance.
        let now = at("2026-11-25T12:00:00Z");
        let codex = |id: &str, peak: f64| (plan("codex", id, "pro", None, 1.0, now), history_of(&[peak; 4], now));
        // 45% then reads 90% of the new allowance: with 10% (20%) beside it, nothing fits.
        assert!(shares(&advice_for("codex", vec![codex("a", 45.0), codex("b", 10.0)], now)).is_empty());
        assert_eq!(shares(&advice_for("codex", vec![codex("a", 30.0), codex("b", 10.0)], now)), [("b", "a", 200.0)]);
    }

    #[test]
    fn no_cut_while_plans_run_out_leaves_less_than_their_busiest_weeks_plus_forty_percent() {
        let now = at("2026-11-25T12:00:00Z");
        // Two plans at their limit: the light two would fit on one, but 2.5 x 1.4 = 3.5 must stay.
        let a = claude_weeks(&[("a", "20x", &[100.0; 4]), ("b", "20x", &[100.0; 4]), ("c", "20x", &[30.0; 4]), ("d", "20x", &[20.0; 4])], now);
        assert!(a.moves.is_empty() && a.shortage.is_none(), "no cut the next advice would ask back: {a:?}");
        assert_eq!(a.floor, Some(3.5));
        // Without a plan at its limit, plus 20%: 3.35 x 1.2 = 4.02 of 5, so the share would cut too far
        // and the downgrade does not.
        let a = claude_weeks(&[("a", "20x", &[30.0; 4]), ("b", "20x", &[20.0; 4]), ("c", "20x", &[95.0; 4]), ("d", "20x", &[95.0; 4]), ("e", "20x", &[95.0; 4])], now);
        assert_eq!(a.moves.iter().map(|m| (m.id.as_str(), m.action)).collect::<Vec<_>>(), [("b", Action::Downgrade)]);
        assert_eq!(a.room.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn the_owners_fleet_shares_nothing_and_still_needs_more() {
        let now = at("2026-11-25T12:00:00Z");
        // Six plans at their limit, one that reached it once, one underused (median 23.5%, average 37.5%).
        let mut fleet: Vec<(&str, &str, &[f64])> = ["p1", "p2", "p3", "p4", "p5", "p6"].iter().map(|id| (*id, "20x", &[100.0; 4][..])).collect();
        fleet.push(("once", "20x", &[100.0, 60.0, 60.0, 60.0]));
        fleet.push(("light", "20x", &[14.0, 22.0, 25.0, 89.0]));
        let a = claude_weeks(&fleet, now);
        assert!(a.moves.is_empty() && a.room.is_empty());
        let s = a.shortage.as_ref().unwrap();
        assert_eq!((s.cause, s.fix.action, s.fix.to.as_str()), (ShortageCause::OverCapacity, FixAction::Add, "Max 20x"));
    }

    /// Advice followed: cancelled and shared plans gone, each kept plan carrying its guests' weeks, a
    /// downgraded one on its new tier with the same use. Claude Max plans with four equal weeks each.
    fn applied(fleet: &[(String, &'static str, f64)], a: &ProviderAdvice) -> Vec<(String, &'static str, f64)> {
        let weight = |tier: &str| if tier == "20x" { 1.0 } else { 0.25 };
        let tier_of = |plan: &str| if plan == "Max 20x" { "20x" } else { "5x" };
        let mut out: Vec<(String, &'static str, f64)> = vec![];
        for (id, tier, peak) in fleet {
            if a.moves.iter().any(|m| &m.id == id && matches!(m.action, Action::Cancel | Action::Share)) { continue; }
            // Use in plans: its own, and every plan's shared onto it.
            let used = peak / 100.0 * weight(tier) + a.moves.iter().filter(|m| m.share.as_ref().is_some_and(|s| &s.id == id))
                .map(|m| { let (_, t, p) = fleet.iter().find(|(g, ..)| g == &m.id).unwrap(); p / 100.0 * weight(t) }).sum::<f64>();
            let tier = a.moves.iter().find(|m| &m.id == id && m.action == Action::Downgrade).map_or(*tier, |m| tier_of(m.to.as_deref().unwrap()));
            out.push((id.clone(), tier, used / weight(tier) * 100.0));
        }
        out
    }

    #[test]
    fn following_the_advice_never_leads_to_advice_to_buy_back() {
        let now = at("2026-11-25T12:00:00Z");
        let peaks = [0.0, 3.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0, 80.0, 100.0];
        let mut checked = 0;
        for (i, a) in peaks.iter().enumerate() {
            for b in &peaks[i..] {
                for c in [10.0, 40.0, 100.0] {
                    for tier in ["20x", "5x"] {
                        let fleet = vec![("a".to_string(), "20x", *a), ("b".to_string(), tier, *b), ("c".to_string(), "20x", c)];
                        let build = |f: &[(String, &'static str, f64)]| {
                            let weeks: Vec<[f64; 4]> = f.iter().map(|(_, _, p)| [*p; 4]).collect();
                            let list: Vec<(&str, &str, &[f64])> = f.iter().zip(&weeks).map(|((id, t, _), w)| (id.as_str(), *t, &w[..])).collect();
                            claude_weeks(&list, now)
                        };
                        let first = build(&fleet);
                        if first.moves.is_empty() { continue; }
                        checked += 1;
                        let next = build(&applied(&fleet, &first));
                        assert!(next.shortage.is_none(), "{fleet:?} → {:?} → {next:?}", first.moves);
                        assert!(next.moves.iter().all(|m| m.action != Action::Share || first.moves.iter().all(|f| f.id != m.share.as_ref().unwrap().id || f.action == Action::Downgrade)),
                            "a share is never undone: {fleet:?}");
                    }
                }
            }
        }
        assert!(checked > 20, "the grid exercises advice ({checked})");
    }

    #[test]
    fn many_plans_are_placed_one_at_a_time_and_still_fit() {
        let now = at("2026-11-25T12:00:00Z");
        let ids: Vec<String> = (0..12).map(|i| format!("p{i:02}")).collect();
        let fleet: Vec<(&str, &str, &[f64])> = ids.iter().map(|id| (id.as_str(), "20x", &[10.0; 4][..])).collect();
        let a = claude_weeks(&fleet, now);
        // Twelve plans at 10%: eight fit on one plan (0.8 x 1.2 = 0.96), so two are kept.
        assert_eq!(shares(&a).len(), 10);
        assert!(a.capacity_after + 1e-9 >= a.floor.unwrap());
        for m in a.moves.iter().filter(|m| m.action == Action::Share) {
            assert!(m.share.as_ref().unwrap().combined_percent * 1.2 <= 100.0 + 1e-6, "{m:?}");
        }
    }

    #[test]
    fn advice_never_cuts_below_the_busiest_weeks_plus_twenty_percent() {
        let now = at("2026-10-21T12:00:00Z");
        // Busiest week 0.9 + 0.15 = 1.05 plans; floor 1.26. Two cancels leave 2; the downgrade would leave 1.25.
        let a = &claude(&[("busy", "20x", 90.0), ("idle1", "20x", 0.0), ("idle2", "20x", 0.0), ("light", "20x", 15.0)], now)[0];
        assert_eq!(a.moves.iter().map(|m| m.action).collect::<Vec<_>>(), [Action::Cancel, Action::Cancel]);
        assert_eq!((a.capacity_after, a.kept_for_capacity), (2.0, 1));
        // Whatever the fleet, what is left covers the floor, and one plan always stays.
        for busy in [0.0, 20.0, 50.0, 80.0, 99.0, 100.0] {
            for light in [3.0, 10.0, 20.0, 25.0] {
                let fleet = [("busy", "20x", busy), ("idle", "20x", 0.0), ("light", "20x", light), ("small", "5x", light), ("idle5", "5x", 0.0)];
                let a = &claude(&fleet, now)[0];
                assert!(a.capacity_after + 1e-9 >= a.floor.unwrap(), "{busy} {light}: {a:?}");
                assert!(a.moves.iter().filter(|m| matches!(m.action, Action::Cancel | Action::Share)).count() < 5);
                for m in a.moves.iter().filter(|m| m.action == Action::Downgrade) {
                    let to = tiers(Provider::Claude).iter().find(|t| Some(t.0) == m.to.as_deref()).unwrap();
                    let from = tiers(Provider::Claude).iter().find(|t| t.0 == m.from).unwrap();
                    assert!(m.peak_percent.unwrap() / 100.0 * from.1 * 1.2 <= to.1 + 1e-9, "the new tier fits the busiest week: {m:?}");
                }
                // A plan kept for others carries them all, plus 20%, on the tier it ends on.
                for m in a.moves.iter().filter(|m| m.action == Action::Share) {
                    let s = m.share.as_ref().unwrap();
                    assert!(s.combined_percent * 1.2 <= 100.0 + 1e-6, "{busy} {light}: {m:?}");
                    assert!(!a.moves.iter().any(|o| o.id == s.id && o.action != Action::Downgrade), "a kept plan is never cut: {a:?}");
                }
            }
        }
        // Every plan idle: all but one go.
        let a = &claude(&[("x", "20x", 0.0), ("y", "20x", 0.0), ("z", "20x", 0.0)], now)[0];
        assert_eq!((a.moves.len(), a.capacity_after, a.kept_for_capacity), (2, 1.0, 1));
    }

    #[test]
    fn a_shortage_gets_the_cheapest_fix_and_never_alongside_a_cut() {
        let now = at("2026-10-21T12:00:00Z");
        // Busiest weeks 2 plans; floor 2.4 over 2 held: add one Max 20x (no tier above to upgrade to).
        let a = &claude(&[("a", "20x", 100.0), ("b", "20x", 100.0)], now)[0];
        let s = a.shortage.as_ref().unwrap();
        assert_eq!((s.cause, s.shortfall, s.exhausted), (ShortageCause::OverCapacity, 0.4, 2));
        assert_eq!((s.fix.action, s.fix.to.as_str(), s.fix.count, s.fix.monthly_cost, s.fix.capacity_after), (FixAction::Add, "Max 20x", 1, 200.0, 3.0));
        assert!(a.moves.is_empty());

        // Short by 0.25: adding a Max 5x and upgrading the Max 5x to 20x both cost $100; the upgrade adds more.
        let a = &claude(&[("a", "20x", 100.0), ("b", "5x", 100.0)], now)[0];
        let f = &a.shortage.as_ref().unwrap().fix;
        assert_eq!((f.action, f.id.as_deref(), f.from.as_deref(), f.to.as_str(), f.monthly_cost, f.capacity_after), (FixAction::Upgrade, Some("b"), Some("Max 5x"), "Max 20x", 100.0, 2.0));
        assert_eq!(f.manage_url, "https://claude.ai/settings/billing");
        // Busiest weeks 1.05 plans, floor 1.26 over 1.25 held: a Pro plan is the cheapest cover.
        let a = &claude(&[("a", "20x", 80.0), ("b", "5x", 100.0)], now)[0];
        let s = a.shortage.as_ref().unwrap();
        assert_eq!((s.shortfall, s.fix.action, s.fix.to.as_str(), s.fix.monthly_cost), (0.01, FixAction::Add, "Pro", 20.0));

        // Within capacity but running into limits with little spare: real demand is cut short, so add.
        let a = &claude(&[("a", "20x", 100.0), ("b", "20x", 100.0), ("c", "20x", 50.0)], now)[0];
        let s = a.shortage.as_ref().unwrap();
        assert_eq!((s.cause, s.shortfall, s.fix.action, s.fix.monthly_cost), (ShortageCause::HittingLimits, 0.5, FixAction::Add, 200.0));
        // With room enough, plans at their limit are a matter of spreading the work: nothing to buy.
        assert!(claude(&[("a", "20x", 100.0), ("b", "20x", 100.0), ("idle", "20x", 0.0)], now)[0].shortage.is_none());
        assert!(claude(&[("a", "20x", 50.0), ("b", "20x", 50.0)], now)[0].shortage.is_none());
        // A cut and an addition never come together.
        for busy in [0.0, 50.0, 100.0] {
            for light in [0.0, 10.0, 100.0] {
                let a = &claude(&[("busy", "20x", busy), ("x", "20x", 100.0), ("idle", "20x", 0.0), ("light", "5x", light)], now)[0];
                assert!(a.moves.is_empty() || a.shortage.is_none(), "{busy} {light}: {a:?}");
            }
        }
    }

    #[test]
    fn busy_healthy_and_ending_plans_are_left_alone() {
        let now = at("2026-10-21T12:00:00Z");
        let a = &claude(&[("busy", "20x", 100.0), ("ok", "20x", 50.0)], now)[0];
        assert!(a.moves.is_empty() && a.monthly_saving == 0.0);
        let mut history = History::default();
        history.accounts.insert("Claude:gone".into(), watched(0.0, now));
        history.accounts.insert("Claude:busy".into(), watched(40.0, now));
        let mut gone = plan("claude", "gone", "max", Some("default_claude_max_20x"), 0.0, now);
        gone.billing.cancels_at = Some(now + Duration::days(4));
        let busy = plan("claude", "busy", "max", Some("default_claude_max_20x"), 0.0, now);
        let r = report(&Snapshot { accounts: vec![gone, busy], updated_at: Some(now) }, &history, &Settings::default(), None, now);
        assert!(r.advice[0].moves.is_empty(), "a plan already cancelled is not advised again");
        assert_eq!(r.advice[0].capacity_now, 1.0);
    }

    #[test]
    fn with_too_little_history_only_clear_evidence_counts() {
        let now = at("2026-10-21T12:00:00Z");
        let mut local = crate::codex_local::Local { attributed_since: Some(now - Duration::days(26)), ..Default::default() };
        local.accounts.insert("acct-busy".into(), crate::codex_local::Activity { threads: 5, tokens: 9000, last: Some(now - Duration::days(1)),
            days: [((now - Duration::days(1)).date_naive(), (5, 9000))].into_iter().collect() });
        // Idle all week: 0% with its reset a full week out.
        let mut idle = plan("codex", "u1:acct-idle", "pro", None, 0.0, now);
        idle.windows[0].resets_at = Some(now + Duration::days(7));
        let busy = plan("codex", "u2:acct-busy", "pro", None, 30.0, now);
        let r = report(&Snapshot { accounts: vec![idle.clone(), busy], updated_at: Some(now) }, &History::default(), &Settings::default(), Some(&local), now);
        let a = &r.advice[0];
        assert_eq!((a.evidence, a.weeks_used, a.demand_p90, a.unit), (Evidence::ClearOnly, 0, None, "Pro 200"));
        assert_eq!(a.moves.len(), 1);
        let m = &a.moves[0];
        assert_eq!((m.id.as_str(), m.action, m.monthly_saving, m.basis), ("u1:acct-idle", Action::Cancel, 200.0, Basis::LocalCodex));
        // This week's use + 20% still fits what is left: 0.3 plans of 1.
        assert_eq!((a.floor, a.capacity_after, a.spare_after), (Some(0.36), 1.0, Some(0.7)));
        // Alone, the idle plan is the provider's last: no advice to drop it.
        let r = report(&Snapshot { accounts: vec![idle], updated_at: Some(now) }, &History::default(), &Settings::default(), Some(&local), now);
        assert!(r.advice[0].moves.is_empty());
        // Underused on thin evidence is never downgraded.
        let mut history = History::default();
        let mut thin = watched(10.0, now);
        thin.windows.truncate(2);
        thin.first_seen = Some(now - Duration::days(2));
        history.accounts.insert("Claude:a".into(), thin.clone());
        history.accounts.insert("Claude:b".into(), thin);
        let accounts = vec![plan("claude", "a", "max", Some("default_claude_max_20x"), 1.0, now), plan("claude", "b", "max", Some("default_claude_max_20x"), 1.0, now)];
        let r = report(&Snapshot { accounts, updated_at: Some(now) }, &history, &Settings::default(), None, now);
        assert_eq!(r.accounts[0].assessment.verdict, Verdict::Underused);
        assert_eq!((r.advice[0].evidence, r.advice[0].moves.len()), (Evidence::ClearOnly, 0));
    }
}
