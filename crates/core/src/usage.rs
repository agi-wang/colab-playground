//! Live compute-unit usage per account.
//!
//! Colab only reports account totals (balance, burn rate, number of
//! assignments). This module turns successive readings into what the UI shows:
//!
//! * [`UsageSnapshot`] — the last reading, marked `stale` when a later fetch
//!   failed. Unknown is never treated as zero.
//! * [`poll_due`] — the cadence from [`Defaults`] (30 s while the account has
//!   sessions, 5 min otherwise by default; manual refresh is always allowed).
//! * [`RateAttribution`] — an *estimated* per-session burn rate, taken from the
//!   change of the account rate observed when sessions start or stop, split
//!   evenly when ambiguous. All per-session figures are estimates (`≈`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::clock::serde_secs;
use crate::defaults::Defaults;
use crate::error::Error;
use crate::sessions::Accelerator;
/// Default capacity of [`BalanceHistory`] (one hour at 30 s).
pub const HISTORY_CAPACITY: usize = 120;

/// A rate (CU/h) at or below this is rounding error, not a runtime burning.
const NO_RATE: f64 = 1e-9;

/// How long the account rate and the runtimes must have stayed as they are
/// for a reading to count as settled. Nothing says how soon Colab's rate
/// follows a runtime that comes or goes, and a reading taken before it has
/// does not hold what the runtimes burn. A time, not a number of readings:
/// how often the app reads is a setting, and it reads again at every change.
const SETTLE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub balance_cu: f64,
    /// Current burn rate of the whole account in CU per hour.
    pub rate_cu_per_hour: f64,
    /// Active assignments (runtimes) on the account, managed or not.
    pub assignments: u32,
}

impl Usage {
    /// The session this reading measures: the only managed session of an
    /// account with a single runtime burns the whole account rate. `None`
    /// while nothing burns (the account rate can lag behind a new runtime).
    pub fn sole_session<'a>(&self, live: &'a BTreeSet<String>) -> Option<&'a str> {
        let burning = self.rate_cu_per_hour.is_finite() && self.rate_cu_per_hour > NO_RATE;
        if self.assignments != 1 || live.len() != 1 || !burning {
            return None;
        }
        live.first().map(String::as_str)
    }
}

/// What users who measured this hardware published as its burn rate (CU/h),
/// for hardware the app has not measured itself. Colab publishes no rates and
/// changes them, so this is only where the figure starts: a reading of the
/// app's own replaces it ([`Usage::sole_session`]).
///
/// From the table measured at
/// <https://mcp-tool-shop-org.github.io/rnd/handbook/colab-golden-path/>; the
/// TPUs from <https://note.com/okakutech/n/nfb368939d8d4>, the high-RAM T4 and
/// the H100 from <https://www.gkbrk.com/google-colab>. The G4 and the H100
/// were published for one machine shape, which stands for both.
pub fn published_rate(accelerator: Accelerator, high_mem: bool) -> f64 {
    let high_mem = high_mem && accelerator.supports_high_mem();
    match (accelerator, high_mem) {
        (Accelerator::None, false) => 0.08,
        (Accelerator::None, true) => 0.26,
        (Accelerator::T4, false) => 1.07,
        (Accelerator::T4, true) => 1.41,
        (Accelerator::L4, _) => 1.54,
        (Accelerator::G4, _) => 8.90,
        (Accelerator::A100, false) => 5.30,
        (Accelerator::A100, true) => 6.77,
        (Accelerator::H100, _) => 18.05,
        (Accelerator::TpuV5e1, _) => 2.92,
        (Accelerator::TpuV6e1, _) => 4.08,
    }
}

/// A burn rate Colab reported, and when.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateReading {
    pub cu_per_hour: f64,
    #[serde(with = "serde_secs")]
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub balance_cu: f64,
    pub rate_cu_per_hour: f64,
    pub assignments: u32,
    #[serde(with = "serde_secs")]
    pub fetched_at: SystemTime,
    /// A later fetch failed: the values are the last known ones.
    pub stale: bool,
}

impl UsageSnapshot {
    pub fn new(usage: Usage, at: SystemTime) -> Self {
        UsageSnapshot {
            balance_cu: usage.balance_cu,
            rate_cu_per_hour: usage.rate_cu_per_hour,
            assignments: usage.assignments,
            fetched_at: at,
            stale: false,
        }
    }

    pub fn usage(&self) -> Usage {
        Usage {
            balance_cu: self.balance_cu,
            rate_cu_per_hour: self.rate_cu_per_hour,
            assignments: self.assignments,
        }
    }

    /// Time until the balance reaches zero at the current rate: `balance / rate`.
    /// `None` when nothing is burning; zero when the balance is exhausted.
    pub fn eta(&self) -> Option<Duration> {
        if self.balance_cu <= 0.0 {
            return Some(Duration::ZERO);
        }
        if self.rate_cu_per_hour <= 0.0 || !self.rate_cu_per_hour.is_finite() {
            return None;
        }
        Some(Duration::from_secs_f64(
            (self.balance_cu / self.rate_cu_per_hour * 3600.0).min(1e12),
        ))
    }

    /// Balance extrapolated to `now` with the sampled rate.
    pub fn projected_balance(&self, now: SystemTime) -> f64 {
        let age_h = now
            .duration_since(self.fetched_at)
            .unwrap_or_default()
            .as_secs_f64()
            / 3600.0;
        self.balance_cu - self.rate_cu_per_hour.max(0.0) * age_h
    }

    /// Not stale and at most `max_age` old.
    pub fn is_fresh(&self, now: SystemTime, max_age: Duration) -> bool {
        !self.stale && now.duration_since(self.fetched_at).unwrap_or_default() <= max_age
    }
}

/// True when a poll is due given the last attempt (successful or not) and
/// the cadence in `defaults`.
pub fn poll_due(
    last_attempt: Option<SystemTime>,
    now: SystemTime,
    has_sessions: bool,
    defaults: &Defaults,
) -> bool {
    match last_attempt {
        None => true,
        Some(t) => {
            now.duration_since(t).unwrap_or_default() >= defaults.usage_poll_interval(has_sessions)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BalancePoint {
    #[serde(with = "serde_secs")]
    pub at: SystemTime,
    pub balance_cu: f64,
}

/// A bounded history of balances (oldest dropped first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceHistory {
    capacity: usize,
    points: VecDeque<BalancePoint>,
}

impl Default for BalanceHistory {
    fn default() -> Self {
        BalanceHistory::with_capacity(HISTORY_CAPACITY)
    }
}

impl BalanceHistory {
    /// An empty history keeping at most `capacity` points (minimum 1).
    pub fn with_capacity(capacity: usize) -> Self {
        BalanceHistory {
            capacity: capacity.max(1),
            points: VecDeque::new(),
        }
    }

    /// Appends a point; a point at the same instant replaces the last one.
    pub fn push(&mut self, at: SystemTime, balance_cu: f64) {
        if let Some(last) = self.points.back_mut()
            && last.at == at
        {
            last.balance_cu = balance_cu;
            return;
        }
        self.points.push_back(BalancePoint { at, balance_cu });
        while self.points.len() > self.capacity {
            self.points.pop_front();
        }
    }

    /// Points, oldest first.
    pub fn points(&self) -> Vec<BalancePoint> {
        self.points.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// Sessions that started between two settled readings, and what they burn
/// together: the rise of the account rate from one reading to the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Split {
    sessions: BTreeSet<String>,
    total: f64,
}

/// What came and went since the last settled reading.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Changes {
    started: BTreeSet<String>,
    stopped: BTreeSet<String>,
    /// How many runtimes came or went, unmanaged ones included.
    runtimes: usize,
    /// In how many readings the account rate moved.
    moves: usize,
    /// These sessions are not all that moves the account rate: the number
    /// of unmanaged runtimes changed, the rate moved more often than
    /// runtimes came or went, or it has yet to follow one that did.
    other: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateAttribution {
    last_rate: Option<f64>,
    rates: BTreeMap<String, Option<f64>>,
    /// Runtimes of the account beyond the managed sessions at the last reading.
    #[serde(default)]
    unmanaged: Option<usize>,
    /// The first of the readings since which the rate and the runtimes have
    /// stayed as they are.
    #[serde(default, with = "serde_secs::option")]
    held_since: Option<SystemTime>,
    /// The account rate at the last settled reading.
    #[serde(default)]
    settled: Option<f64>,
    #[serde(default)]
    changes: Changes,
    #[serde(default)]
    splits: Vec<Split>,
    /// What the last session of a split burns, once the others have stopped.
    /// `rates` stays as it was estimated, so that every other estimate is
    /// worked out as if nothing had been measured.
    #[serde(default)]
    measured: BTreeMap<String, f64>,
}

impl RateAttribution {
    /// `live` are the app-managed sessions running now. Sessions that appeared
    /// since the previous reading share the rate increase (plus whatever the
    /// sessions that disappeared were estimated at) evenly. Sessions without
    /// an estimate (first reading, no positive increase) share whatever rate
    /// is not yet explained, evenly with the unmanaged runtimes
    /// (`assignments` beyond `live`). A zero share leaves a session unknown,
    /// to be worked out again at the next reading: the account rate can lag
    /// behind a new runtime, and a zero kept for good would hide its spend
    /// from the cost guard.
    fn observe(&mut self, account_rate: f64, assignments: u32, live: &BTreeSet<String>) {
        let rate = account_rate.max(0.0);
        let mut freed = 0.0;
        self.rates.retain(|name, r| {
            if live.contains(name) {
                true
            } else {
                freed += r.unwrap_or(0.0);
                false
            }
        });
        let started: Vec<String> = live
            .iter()
            .filter(|n| !self.rates.contains_key(*n))
            .cloned()
            .collect();
        let delta_share = match self.last_rate {
            Some(prev) if !started.is_empty() => {
                let delta = rate - prev + freed;
                (delta > NO_RATE).then(|| delta / started.len() as f64)
            }
            _ => None,
        };
        for name in started {
            self.rates.insert(name, delta_share);
        }
        let unknown: Vec<String> = self
            .rates
            .iter()
            // A cache written before zero shares were left unknown can hold one.
            .filter(|(_, r)| r.is_none_or(|r| r <= NO_RATE))
            .map(|(n, _)| n.clone())
            .collect();
        if !unknown.is_empty() {
            let known: f64 = self.rates.values().flatten().sum();
            let unmanaged = (assignments as usize).saturating_sub(live.len());
            let share = (rate - known).max(0.0) / (unknown.len() + unmanaged) as f64;
            if share > NO_RATE {
                for name in unknown {
                    self.rates.insert(name, Some(share));
                }
            }
        }
        self.last_rate = Some(rate);
    }

    /// [`Self::observe`], and a measurement for the estimate that an even
    /// share gets most wrong: that of a session left alone by the sessions it
    /// started with.
    ///
    /// Only settled readings are measured from, since the account rate can
    /// lag behind a runtime that comes or goes: a reading is settled once
    /// the rate and the runtimes have stayed as they are for [`SETTLE`]. The
    /// rise between two settled readings between which only sessions
    /// started is what those sessions burn together, and the fall between
    /// two between which only some of them stopped is what these burned. The
    /// last one left burns the rest. Whatever else happens between two
    /// settled readings measures nothing, and no other estimate changes.
    pub fn observe_at(
        &mut self,
        at: SystemTime,
        account_rate: f64,
        assignments: u32,
        live: &BTreeSet<String>,
    ) {
        let rate = account_rate.max(0.0);
        let unmanaged = (assignments as usize).saturating_sub(live.len());
        let before = self.unmanaged.replace(unmanaged).unwrap_or(unmanaged);
        let mut came_or_went = unmanaged.abs_diff(before);
        for name in self.rates.keys().filter(|name| !live.contains(*name)) {
            came_or_went += 1;
            self.changes.stopped.insert(name.clone());
        }
        for name in live.iter().filter(|name| !self.rates.contains_key(*name)) {
            came_or_went += 1;
            self.changes.started.insert(name.clone());
        }
        let moved = self
            .last_rate
            .is_none_or(|last| (last - rate).abs() > NO_RATE);
        self.changes.other |= unmanaged != before;
        self.changes.runtimes += came_or_went;
        self.changes.moves += usize::from(moved);
        if self.changes.moves > self.changes.runtimes {
            // A runtime that comes or goes moves the rate once. A move
            // beyond that is for one that came or went before the last
            // settled reading, which then did not hold all of what the
            // runtimes burned, or for an unmanaged runtime that took the
            // place of another between two readings.
            self.splits.clear();
            self.changes.other = true;
        }
        if moved || came_or_went > 0 {
            self.held_since = Some(at);
        }
        self.measured.retain(|name, _| live.contains(name));
        self.observe(account_rate, assignments, live);
        let held = self
            .held_since
            .and_then(|since| at.duration_since(since).ok())
            .unwrap_or_default();
        // An account that runs nothing and burns nothing has nothing on its way.
        let idle = assignments == 0 && live.is_empty() && rate <= NO_RATE;
        if held >= SETTLE || idle {
            self.settle(rate, unmanaged);
        }
    }

    /// A settled reading: `rate` is what the runtimes of the account burn.
    fn settle(&mut self, rate: f64, unmanaged: usize) {
        let Some(before) = self.settled else {
            // The first settled reading: the sessions burn all of the rate
            // unless an unmanaged runtime burns part of it.
            self.settled = Some(rate);
            self.changes = Changes::default();
            if unmanaged == 0 {
                self.split(self.rates.keys().cloned().collect(), rate);
            }
            return;
        };
        let rise = rate - before;
        let Changes {
            started,
            stopped,
            runtimes,
            moves,
            other,
        } = std::mem::take(&mut self.changes);
        // A split that lost a session no longer burns its total.
        let (lost, kept): (Vec<Split>, Vec<Split>) = std::mem::take(&mut self.splits)
            .into_iter()
            .partition(|split| !split.sessions.is_disjoint(&stopped));
        self.splits = kept;
        if moves == 0 && (other || started.is_empty() != stopped.is_empty()) {
            // The rate has yet to follow what came or went, so its next
            // move is not all of what comes or goes then.
            self.changes = Changes {
                runtimes,
                moves,
                other: true,
                ..Changes::default()
            };
            return;
        }
        self.settled = Some(rate);
        if other {
            return;
        }
        if stopped.is_empty() {
            self.split(started, rise);
        } else if started.is_empty()
            && rise < -NO_RATE
            && let Some(split) = lost
                .into_iter()
                .find(|split| stopped.is_subset(&split.sessions))
        {
            // The fall is what the stopped sessions burned.
            let total = split.total + rise;
            let mut left = split.sessions;
            left.retain(|name| !stopped.contains(name));
            if left.len() > 1 {
                self.split(left, total);
            } else if let Some(name) = left.into_iter().next()
                && total > NO_RATE
            {
                self.measured.insert(name, total);
            }
        }
    }

    fn split(&mut self, sessions: BTreeSet<String>, total: f64) {
        if sessions.len() > 1 {
            self.splits.push(Split { sessions, total });
        }
    }

    /// Estimated rate of a session (CU/h).
    pub fn rate_of(&self, name: &str) -> Option<f64> {
        self.measured
            .get(name)
            .copied()
            .or_else(|| self.rates.get(name).copied().flatten())
    }

    pub fn rates(&self) -> BTreeMap<String, f64> {
        self.rates
            .keys()
            .filter_map(|n| self.rate_of(n).map(|r| (n.clone(), r)))
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub snapshot: Option<UsageSnapshot>,
    /// Managed runtimes when the snapshot was recorded; absent in older caches.
    #[serde(default)]
    pub managed_at_reading: Option<usize>,
    #[serde(default)]
    pub ownership_verified: bool,
    pub history: BalanceHistory,
    pub attribution: RateAttribution,
    /// Last fetch attempt (successful or not), for [`poll_due`].
    #[serde(default, with = "serde_secs::option")]
    pub last_attempt: Option<SystemTime>,
    pub last_error: Option<Error>,
}

impl AccountUsage {
    pub fn record_success(&mut self, usage: Usage, now: SystemTime, live: &BTreeSet<String>) {
        self.snapshot = Some(UsageSnapshot::new(usage, now));
        self.managed_at_reading = Some(live.len());
        self.ownership_verified = true;
        self.history.push(now, usage.balance_cu);
        self.attribution
            .observe_at(now, usage.rate_cu_per_hour, usage.assignments, live);
        self.last_attempt = Some(now);
        self.last_error = None;
    }

    pub fn record_failure(&mut self, error: Error, now: SystemTime) {
        if let Some(s) = self.snapshot.as_mut() {
            s.stale = true;
        }
        self.last_attempt = Some(now);
        self.last_error = Some(error);
    }

    /// Runtimes beyond those managed at the reading (`known` is a legacy-cache fallback).
    pub fn unmanaged_runtimes(&self, known: usize) -> u32 {
        self.snapshot
            .map(|s| {
                s.assignments
                    .saturating_sub(self.managed_at_reading.unwrap_or(known) as u32)
            })
            .unwrap_or(0)
    }
}

/// Sums balances and rates of the accounts whose usage is known. Unknown
/// accounts are skipped, never counted as zero; `complete` says whether all were known.
pub fn totals<'a>(snapshots: impl IntoIterator<Item = Option<&'a UsageSnapshot>>) -> UsageTotals {
    let mut t = UsageTotals {
        balance_cu: 0.0,
        rate_cu_per_hour: 0.0,
        accounts: 0,
        complete: true,
        any_stale: false,
    };
    for s in snapshots {
        match s {
            Some(s) => {
                t.balance_cu += s.balance_cu;
                t.rate_cu_per_hour += s.rate_cu_per_hour;
                t.accounts += 1;
                t.any_stale |= s.stale;
            }
            None => t.complete = false,
        }
    }
    t
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub balance_cu: f64,
    pub rate_cu_per_hour: f64,
    pub accounts: u32,
    /// False if some account's usage is unknown.
    pub complete: bool,
    /// True if some included snapshot is stale.
    pub any_stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + s)
    }
    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    fn u(b: f64, r: f64, n: u32) -> Usage {
        Usage {
            balance_cu: b,
            rate_cu_per_hour: r,
            assignments: n,
        }
    }

    #[test]
    fn cadence() {
        let d = Defaults::default();
        assert!(poll_due(None, t(0), false, &d));
        assert!(!poll_due(Some(t(0)), t(29), true, &d));
        assert!(poll_due(Some(t(0)), t(30), true, &d));
        assert!(!poll_due(Some(t(0)), t(299), false, &d));
        assert!(poll_due(Some(t(0)), t(300), false, &d));
        let fast = Defaults {
            usage_poll_active: Duration::from_secs(5),
            ..Defaults::default()
        };
        assert!(poll_due(Some(t(0)), t(5), true, &fast));
    }

    #[test]
    fn a_reading_measures_the_only_runtime_of_an_account() {
        let one = set(&["s1"]);
        assert_eq!(u(50.0, 4.82, 1).sole_session(&one), Some("s1"));
        // A runtime the app does not manage, a second session, or no rate yet.
        assert_eq!(u(50.0, 6.58, 2).sole_session(&one), None);
        assert_eq!(u(50.0, 6.58, 2).sole_session(&set(&["s1", "s2"])), None);
        assert_eq!(u(50.0, 0.0, 1).sole_session(&one), None);
        assert_eq!(u(50.0, 4.82, 1).sole_session(&set(&[])), None);
    }

    #[test]
    fn published_rates_follow_the_machine_shape() {
        assert_eq!(published_rate(Accelerator::A100, false), 5.30);
        assert_eq!(published_rate(Accelerator::A100, true), 6.77);
        // An L4 has one machine shape, so the high-RAM option changes nothing.
        assert_eq!(
            published_rate(Accelerator::L4, true),
            published_rate(Accelerator::L4, false)
        );
    }

    #[test]
    fn eta_and_projection() {
        let s = UsageSnapshot::new(u(10.0, 4.0, 1), t(0));
        assert_eq!(s.eta(), Some(Duration::from_secs(9000)));
        assert_eq!(UsageSnapshot::new(u(10.0, 0.0, 0), t(0)).eta(), None);
        assert_eq!(
            UsageSnapshot::new(u(0.0, 4.0, 1), t(0)).eta(),
            Some(Duration::ZERO)
        );
        assert!((s.projected_balance(t(1800)) - 8.0).abs() < 1e-9);
        assert!(s.is_fresh(t(60), Duration::from_secs(300)));
        assert!(!s.is_fresh(t(301), Duration::from_secs(300)));
    }

    #[test]
    fn failure_marks_stale_and_keeps_values() {
        let mut a = AccountUsage::default();
        a.record_failure(Error::new(crate::ErrorKind::Http), t(0));
        assert!(a.snapshot.is_none(), "unknown stays unknown, not zero");
        a.record_success(u(50.0, 5.0, 1), t(30), &set(&["s1"]));
        a.record_failure(Error::new(crate::ErrorKind::Http), t(60));
        let s = a.snapshot.unwrap();
        assert!(s.stale);
        assert_eq!(s.balance_cu, 50.0);
        assert_eq!(a.last_attempt, Some(t(60)));
        assert!(!s.is_fresh(t(61), Duration::from_secs(300)));
    }

    #[test]
    fn history_is_a_ring_buffer() {
        let mut h = BalanceHistory::with_capacity(3);
        for i in 0..5 {
            h.push(t(i), i as f64);
        }
        h.push(t(4), 40.0);
        let p = h.points();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].balance_cu, 2.0);
        assert_eq!(p[2].balance_cu, 40.0);
    }

    #[test]
    fn attribution_from_rate_deltas() {
        let mut a = RateAttribution::default();
        a.observe(0.0, 0, &set(&[]));
        // s1 starts: rate goes 0 -> 11.8, all of it is s1.
        a.observe(11.8, 1, &set(&["s1"]));
        assert_eq!(a.rate_of("s1"), Some(11.8));
        // s2 (a T4) starts: +1.8.
        a.observe(13.6, 2, &set(&["s1", "s2"]));
        assert!((a.rate_of("s2").unwrap() - 1.8).abs() < 1e-9);
        assert_eq!(a.rate_of("s1"), Some(11.8));
        // s1 stops and s3 starts in the same interval: s3 gets the change plus s1's freed rate.
        a.observe(7.0, 2, &set(&["s2", "s3"]));
        assert!((a.rate_of("s3").unwrap() - 5.2).abs() < 1e-9);
        assert_eq!(a.rate_of("s1"), None);
    }

    #[test]
    fn ambiguous_starts_split_evenly_with_unmanaged() {
        let mut a = RateAttribution::default();
        // First reading ever: two sessions plus one unmanaged runtime share 9 CU/h.
        a.observe(9.0, 3, &set(&["a", "b"]));
        assert_eq!(a.rate_of("a"), Some(3.0));
        assert_eq!(a.rate_of("b"), Some(3.0));
        // Two start together: the increase is split evenly.
        a.observe(13.0, 5, &set(&["a", "b", "c", "d"]));
        assert_eq!(a.rate_of("c"), Some(2.0));
        assert_eq!(a.rate_of("d"), Some(2.0));
        // A start without a rate increase (already billed at the previous
        // reading) falls back to the unexplained remainder.
        let mut b = RateAttribution::default();
        b.observe(5.0, 1, &set(&[]));
        b.observe(5.0, 1, &set(&["late"]));
        assert_eq!(b.rate_of("late"), Some(5.0));
    }

    /// Readings as the app takes them: one at every change and one every
    /// 30 s besides. `after` is the time since the reading before.
    #[derive(Default)]
    struct Readings {
        attribution: RateAttribution,
        now: u64,
    }

    impl Readings {
        fn read(&mut self, after: u64, rate: f64, assignments: u32, live: &[&str]) {
            self.now += after;
            self.attribution
                .observe_at(t(self.now), rate, assignments, &set(live));
        }

        /// A reading, and the same one every 30 s until it is settled.
        fn hold(&mut self, after: u64, rate: f64, assignments: u32, live: &[&str]) {
            self.read(after, rate, assignments, live);
            for _ in 0..4 {
                self.read(30, rate, assignments, live);
            }
        }

        /// The estimate of a session, to a millionth of a CU/h.
        fn rate(&self, name: &str) -> Option<f64> {
            self.attribution
                .rate_of(name)
                .map(|rate| (rate * 1e6).round() / 1e6)
        }
    }

    #[test]
    fn a_session_left_by_the_one_it_started_with_is_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An A100 (11.77) and a T4 (1.76) start seconds apart, and the
        // account rate follows half a minute later.
        r.read(60, 0.0, 1, &["a100"]);
        r.read(5, 0.0, 2, &["a100", "t4"]);
        r.hold(30, 13.53, 2, &["a100", "t4"]);
        assert_eq!(r.rate("a100"), Some(6.765));
        assert_eq!(r.rate("t4"), Some(6.765));
        // The A100 stops minutes later, and the rate falls a reading after.
        r.read(300, 13.53, 1, &["t4"]);
        r.read(30, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(6.765));
        r.hold(30, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
    }

    #[test]
    fn a_reading_is_settled_by_the_time_it_held_not_by_how_many_readings() {
        // Read every 5 s, the rate holds for 115 s and for 120 s before the
        // A100 stops.
        for (readings, t4) in [(23, 6.765), (24, 1.76)] {
            let mut r = Readings::default();
            r.read(0, 0.0, 0, &[]);
            r.read(60, 13.53, 2, &["a100", "t4"]);
            for _ in 0..readings {
                r.read(5, 13.53, 2, &["a100", "t4"]);
            }
            r.hold(5, 1.76, 1, &["t4"]);
            assert_eq!(r.rate("t4"), Some(t4));
        }
    }

    #[test]
    fn a_rise_that_shows_in_two_steps_is_one_split() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An A100 and an H100 (18.3) start; the H100's rate shows half a
        // minute later, the A100's 110 s after that.
        r.read(60, 0.0, 2, &["a100", "h100"]);
        for _ in 0..4 {
            r.read(30, 18.30, 2, &["a100", "h100"]);
        }
        r.hold(20, 30.07, 2, &["a100", "h100"]);
        assert_eq!(r.rate("h100"), Some(9.15));
        r.hold(300, 18.30, 1, &["h100"]);
        assert_eq!(r.rate("h100"), Some(18.30));
    }

    #[test]
    fn the_first_reading_of_the_app_is_not_settled() {
        // The app starts while an A100 and an H100 run, and the A100's rate
        // has yet to show. An L4 (4.82) starts as it does.
        let mut r = Readings::default();
        r.read(0, 18.30, 2, &["a100", "h100"]);
        r.hold(30, 34.89, 3, &["a100", "h100", "l4"]);
        // The A100 stops: two are left of the three, so nothing is measured.
        r.hold(300, 23.12, 2, &["h100", "l4"]);
        assert_eq!(r.rate("h100"), Some(9.15));
        r.hold(300, 18.30, 1, &["h100"]);
        assert_eq!(r.rate("h100"), Some(18.30));
    }

    #[test]
    fn sessions_found_beside_an_unmanaged_runtime_are_not_a_split() {
        // The app starts while two T4s run beside an unmanaged A100.
        let mut r = Readings::default();
        r.hold(0, 15.29, 3, &["t4a", "t4b"]);
        r.hold(300, 13.53, 2, &["t4b"]);
        assert_eq!(r.rate("t4b"), Some(5.096667));
    }

    #[test]
    fn a_stop_as_an_unmanaged_runtime_comes_is_not_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An A100 and an L4 start together.
        r.hold(60, 16.59, 2, &["a100", "l4"]);
        // An unmanaged L4 starts; the account rate lags behind it, and
        // shows it as the A100 stops: the fall is 6.95.
        r.read(300, 16.59, 3, &["a100", "l4"]);
        r.hold(30, 9.64, 2, &["l4"]);
        assert_eq!(r.rate("l4"), Some(8.295));
    }

    #[test]
    fn a_stop_is_measured_once_an_unmanaged_runtime_has_shown() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 16.59, 2, &["a100", "l4"]);
        r.read(300, 16.59, 3, &["a100", "l4"]);
        r.hold(30, 21.41, 3, &["a100", "l4"]);
        r.hold(300, 9.64, 2, &["l4"]);
        assert_eq!(r.rate("l4"), Some(4.82));
    }

    #[test]
    fn an_unmanaged_runtime_that_comes_is_waited_for_from_then() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.read(60, 4.82, 1, &["l4a"]);
        r.read(30, 4.82, 1, &["l4a"]);
        // An unmanaged T4 comes a minute after the L4, and its rate shows
        // a minute and a half later, as an A100 and an L4 start.
        r.read(30, 4.82, 2, &["l4a"]);
        r.read(30, 4.82, 2, &["l4a"]);
        r.read(30, 4.82, 2, &["l4a"]);
        r.hold(30, 23.17, 4, &["a100", "l4a", "l4b"]);
        r.hold(300, 11.40, 3, &["l4a", "l4b"]);
        assert_eq!(r.rate("l4b"), Some(9.175));
    }

    #[test]
    fn a_stop_the_rate_rises_at_is_not_measured() {
        // Two L4s start beside an unmanaged T4.
        let mut r = Readings::default();
        r.hold(0, 1.76, 1, &[]);
        r.hold(60, 11.40, 3, &["l4a", "l4b"]);
        // One stops, and an H100 takes the place of the T4 between two
        // readings: as many unmanaged runtimes as before.
        r.hold(300, 23.12, 2, &["l4b"]);
        assert_eq!(r.rate("l4b"), Some(4.82));
    }

    #[test]
    fn a_stop_beside_a_start_is_not_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 13.53, 2, &["a100", "t4"]);
        // The A100 stops and an L4 starts within the same two minutes.
        r.read(300, 1.76, 1, &["t4"]);
        r.hold(60, 6.58, 2, &["l4", "t4"]);
        assert_eq!(r.rate("t4"), Some(6.765));
    }

    #[test]
    fn a_split_ends_when_one_of_its_sessions_stops_unmeasured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 23.12, 2, &["h100", "l4"]);
        // The L4 stops beside a start, then a session takes its name.
        r.read(300, 18.30, 1, &["h100"]);
        r.hold(60, 18.38, 2, &["cpu", "h100"]);
        r.hold(300, 20.14, 3, &["cpu", "h100", "l4"]);
        r.hold(300, 18.38, 2, &["cpu", "h100"]);
        assert_eq!(r.rate("h100"), Some(11.56));
    }

    #[test]
    fn stops_in_two_splits_are_not_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 20.06, 2, &["h100", "t4"]);
        r.hold(300, 29.70, 4, &["h100", "l4a", "l4b", "t4"]);
        // The T4 and an L4 stop within the same two minutes: the fall is
        // what both burned.
        r.read(300, 27.94, 3, &["h100", "l4a", "l4b"]);
        r.hold(60, 23.12, 2, &["h100", "l4b"]);
        assert_eq!(r.rate("h100"), Some(10.03));
        assert_eq!(r.rate("l4b"), Some(4.82));
    }

    #[test]
    fn a_stop_beside_a_stop_outside_the_split_is_not_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 4.82, 1, &["l4"]);
        r.hold(300, 24.88, 3, &["h100", "l4", "t4"]);
        r.read(300, 23.12, 2, &["h100", "l4"]);
        r.hold(60, 18.30, 1, &["h100"]);
        assert_eq!(r.rate("h100"), Some(10.03));
    }

    #[test]
    fn a_split_of_three_is_measured_down_to_its_last_session() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 18.35, 3, &["a100", "l4", "t4"]);
        r.hold(300, 6.58, 2, &["l4", "t4"]);
        assert_eq!(r.rate("l4"), Some(6.116667));
        assert_eq!(r.rate("t4"), Some(6.116667));
        r.hold(300, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
    }

    #[test]
    fn a_rate_that_moves_on_its_own_ends_every_split() {
        // The app starts while an A100 and an H100 run, and the A100's rate
        // shows only after the reading has settled.
        let mut r = Readings::default();
        r.hold(0, 18.30, 2, &["a100", "h100"]);
        r.hold(30, 30.07, 2, &["a100", "h100"]);
        r.hold(300, 18.30, 1, &["h100"]);
        assert_eq!(r.rate("h100"), Some(9.15));
    }

    #[test]
    fn a_rate_that_moves_more_often_than_runtimes_came_or_went_ends_every_split() {
        // An unmanaged H100 and an unmanaged CPU.
        let mut r = Readings::default();
        r.hold(0, 18.38, 2, &[]);
        // Two L4s start seconds apart, and the rate follows each at once.
        r.read(60, 23.20, 3, &["l4a"]);
        r.read(5, 28.02, 4, &["l4a", "l4b"]);
        // A T4 takes the place of the CPU between two readings: as many
        // unmanaged runtimes as before.
        r.hold(30, 29.70, 4, &["l4a", "l4b"]);
        r.hold(300, 24.88, 3, &["l4b"]);
        assert_eq!(r.rate("l4b"), Some(4.82));
    }

    #[test]
    fn a_rate_that_follows_late_leaves_the_splits_of_other_sessions() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 13.53, 2, &["a100", "t4"]);
        // An L4 starts, and the rate follows it after more than two minutes.
        r.hold(300, 13.53, 3, &["a100", "l4", "t4"]);
        r.hold(30, 18.35, 3, &["a100", "l4", "t4"]);
        r.hold(300, 6.58, 2, &["l4", "t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
    }

    #[test]
    fn a_runtime_that_came_and_went_leaves_nothing_on_its_way() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An unmanaged T4 comes and goes, and the account burns nothing again.
        r.read(60, 1.76, 1, &[]);
        r.read(60, 0.0, 0, &[]);
        r.hold(60, 13.53, 2, &["a100", "t4"]);
        r.hold(300, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
    }

    #[test]
    fn a_rise_after_a_start_the_rate_did_not_follow_is_not_a_split() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An A100 starts, and the rate has not followed after two minutes.
        r.hold(60, 0.0, 1, &["a100"]);
        // It does as an L4 and a T4 start.
        r.read(30, 11.77, 3, &["a100", "l4", "t4"]);
        r.hold(30, 18.35, 3, &["a100", "l4", "t4"]);
        r.hold(300, 13.53, 2, &["a100", "t4"]);
        assert_eq!(r.rate("t4"), Some(5.885));
    }

    #[test]
    fn a_rise_after_an_unmanaged_runtime_the_rate_did_not_follow_is_not_a_split() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        // An unmanaged L4 comes, and the rate has not followed after two
        // minutes. It does as an A100 and a T4 start.
        r.hold(60, 0.0, 1, &[]);
        r.read(30, 4.82, 3, &["a100", "t4"]);
        r.hold(30, 18.35, 3, &["a100", "t4"]);
        r.hold(300, 6.58, 2, &["t4"]);
        assert_eq!(r.rate("t4"), Some(2.41));
    }

    #[test]
    fn a_fall_after_a_stop_the_rate_did_not_follow_is_not_measured() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 13.53, 2, &["a100", "t4"]);
        // The A100 stops, and the rate has not followed after two minutes.
        r.hold(300, 13.53, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(6.765));
        r.hold(30, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(6.765));
    }

    #[test]
    fn an_account_that_runs_and_burns_nothing_is_settled_at_once() {
        let mut r = Readings::default();
        r.hold(0, 4.82, 1, &["l4"]);
        // The L4 stops, and an A100 and a T4 start half a minute later.
        r.read(300, 0.0, 0, &[]);
        r.hold(30, 13.53, 2, &["a100", "t4"]);
        r.hold(300, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
    }

    #[test]
    fn starts_beside_a_stop_are_not_a_split() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 1.76, 1, &["t4"]);
        // The T4 stops and an A100 and an L4 start: the rise is 14.83.
        r.hold(300, 16.59, 2, &["a100", "l4"]);
        r.hold(300, 4.82, 1, &["l4"]);
        assert_eq!(r.rate("l4"), Some(8.295));
    }

    #[test]
    fn an_account_with_a_rate_or_a_runtime_on_its_way_is_not_settled_at_once() {
        // The app starts on an empty account that still burns what a T4
        // did, and an A100 and an L4 start: the rise is 14.83, not what
        // they burn.
        let mut r = Readings::default();
        r.read(0, 1.76, 0, &[]);
        r.hold(30, 16.59, 2, &["a100", "l4"]);
        r.hold(300, 4.82, 1, &["l4"]);
        assert_eq!(r.rate("l4"), Some(4.82));

        // The app starts beside an unmanaged L4 whose rate has yet to show.
        let mut r = Readings::default();
        r.read(0, 0.0, 1, &[]);
        r.hold(30, 18.35, 3, &["a100", "t4"]);
        r.hold(300, 6.58, 2, &["t4"]);
        assert_eq!(r.rate("t4"), Some(9.175));

        // The app starts with an A100 that neither the count of runtimes
        // nor the rate shows yet.
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &["a100"]);
        r.hold(30, 18.35, 3, &["a100", "l4", "t4"]);
        r.hold(300, 13.53, 2, &["a100", "t4"]);
        assert_eq!(r.rate("t4"), Some(9.175));
    }

    #[test]
    fn a_measurement_ends_with_its_session() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 13.53, 2, &["a100", "t4"]);
        r.hold(300, 1.76, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
        // It stops, and an H100 takes its name.
        r.read(300, 0.0, 0, &[]);
        r.read(60, 18.30, 1, &["t4"]);
        assert_eq!(r.rate("t4"), Some(18.30));
    }

    #[test]
    fn a_fall_that_leaves_nothing_of_a_split_measures_no_zero() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 3.52, 2, &["t4a", "t4b"]);
        // One stops, and the account burns nothing though the other runs.
        r.hold(300, 0.0, 1, &["t4a"]);
        assert_eq!(r.rate("t4a"), Some(1.76));
    }

    #[test]
    fn a_session_on_its_own_is_not_kept_as_a_split() {
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 4.82, 1, &["l4"]);
        r.hold(300, 6.58, 2, &["l4", "t4"]);
        let state = serde_json::to_value(&r.attribution).unwrap();
        assert_eq!(state["splits"], serde_json::json!([]));
    }

    #[test]
    fn the_cases_of_the_issue_are_corrected_minutes_after_the_start() {
        // A T4 and a CPU (0.08) start beside an A100, the T4 stops.
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 11.77, 1, &["a100"]);
        r.hold(300, 13.61, 3, &["a100", "cpu", "t4"]);
        assert_eq!(r.rate("cpu"), Some(0.92));
        r.hold(300, 11.85, 2, &["a100", "cpu"]);
        assert_eq!(r.rate("cpu"), Some(0.08));
        assert_eq!(r.rate("a100"), Some(11.77));

        // Two CPUs start together, then an A100 and a T4; the A100 stops.
        let mut r = Readings::default();
        r.read(0, 0.0, 0, &[]);
        r.hold(60, 0.16, 2, &["cpu1", "cpu2"]);
        r.hold(300, 13.69, 4, &["a100", "cpu1", "cpu2", "t4"]);
        assert_eq!(r.rate("t4"), Some(6.765));
        r.hold(300, 1.92, 3, &["cpu1", "cpu2", "t4"]);
        assert_eq!(r.rate("t4"), Some(1.76));
        assert_eq!(r.rate("cpu1"), Some(0.08));
    }

    #[test]
    fn what_was_held_and_split_survives_a_restart_of_the_app() {
        let live = set(&["a100", "t4"]);
        let mut a = AccountUsage::default();
        a.record_success(u(90.0, 0.0, 0), t(0), &set(&[]));
        for at in [60, 90, 120, 150] {
            a.record_success(u(90.0, 13.53, 2), t(at), &live);
        }
        let mut a: AccountUsage =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        a.record_success(u(90.0, 13.53, 2), t(180), &live);
        for at in [600, 630, 660, 690] {
            a.record_success(u(89.0, 1.76, 1), t(at), &set(&["t4"]));
        }
        let mut a: AccountUsage =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        a.record_success(u(89.0, 1.76, 1), t(720), &set(&["t4"]));
        assert!((a.attribution.rate_of("t4").unwrap() - 1.76).abs() < 1e-9);
    }

    #[test]
    fn a_cache_written_before_readings_were_settled_measures_nothing_wrong() {
        // Two sessions with shares of 9.15, and nothing that says whether
        // the 18.30 held all of what they burn.
        let mut a: RateAttribution = serde_json::from_value(serde_json::json!({
            "last_rate": 18.30,
            "rates": {"a100": 9.15, "h100": 9.15},
        }))
        .unwrap();
        a.observe_at(t(0), 30.07, 2, &set(&["a100", "h100"]));
        a.observe_at(t(30), 18.30, 1, &set(&["h100"]));
        for at in [60, 90, 120, 150] {
            a.observe_at(t(at), 18.30, 1, &set(&["h100"]));
        }
        assert_eq!(a.rate_of("h100"), Some(9.15));
    }

    #[test]
    fn a_start_the_rate_lags_behind_stays_unknown_until_its_rate_shows() {
        let mut a = RateAttribution::default();
        a.observe(0.0, 0, &set(&[]));
        a.observe(0.0, 1, &set(&["h"]));
        assert_eq!(a.rate_of("h"), None);
        a.observe(0.0, 1, &set(&["h"]));
        assert_eq!(a.rate_of("h"), None);
        a.observe(18.3, 1, &set(&["h"]));
        assert_eq!(a.rate_of("h"), Some(18.3));
        // Beside a known session it gets the remainder once that shows.
        a.observe(18.3, 2, &set(&["h", "t"]));
        assert_eq!(a.rate_of("t"), None);
        a.observe(20.06, 2, &set(&["h", "t"]));
        assert!((a.rate_of("t").unwrap() - 1.76).abs() < 1e-9);
        assert_eq!(a.rate_of("h"), Some(18.3));
        // On the first reading too.
        let mut first = RateAttribution::default();
        first.observe(0.0, 1, &set(&["x"]));
        assert_eq!(first.rate_of("x"), None);
        assert!(first.rates().is_empty());
    }

    #[test]
    fn a_lagging_start_beside_an_unmanaged_runtime_takes_its_share_at_once() {
        // An estimate the spend cap can stop on, rather than unknown for as long as it runs.
        let mut a = RateAttribution::default();
        a.observe(5.0, 1, &set(&[]));
        a.observe(5.0, 2, &set(&["m"]));
        assert_eq!(a.rate_of("m"), Some(2.5));
    }

    #[test]
    fn spend_of_a_start_the_rate_lags_behind_counts_from_its_start() {
        use crate::guard::{Concern, Decision, GuardPolicy, Limit, SpendMeter, evaluate};
        let policy = GuardPolicy {
            max_spend_cu: Some(9.0),
            ..GuardPolicy::unlimited()
        };
        let mut a = RateAttribution::default();
        let mut meter = SpendMeter::new(t(0));
        a.observe(0.0, 0, &set(&[]));
        a.observe(0.0, 1, &set(&["h"]));
        meter.record(t(30), a.rate_of("h"));
        assert_eq!(
            evaluate(&policy, t(30), None, &meter),
            Decision::Warn(vec![Concern::UsageUnknown {
                limit: Limit::Spend
            }])
        );
        a.observe(18.3, 1, &set(&["h"]));
        meter.record(t(1800), a.rate_of("h"));
        assert!((meter.spend().unwrap() - 9.15).abs() < 1e-9);
        assert!(matches!(
            evaluate(&policy, t(1800), None, &meter),
            Decision::Stop(_)
        ));
    }

    #[test]
    fn a_zero_share_from_an_older_cache_is_worked_out_again() {
        let mut a: RateAttribution =
            serde_json::from_value(serde_json::json!({"last_rate": 0.0, "rates": {"h": 0.0}}))
                .unwrap();
        a.observe(18.3, 1, &set(&["h"]));
        assert_eq!(a.rate_of("h"), Some(18.3));
    }

    #[test]
    fn unmanaged_and_totals() {
        let mut a = AccountUsage::default();
        a.record_success(u(80.0, 13.6, 3), t(0), &set(&["s1", "s2"]));
        assert_eq!(a.unmanaged_runtimes(2), 1);
        let s1 = UsageSnapshot::new(u(10.0, 1.0, 1), t(0));
        let mut s2 = UsageSnapshot::new(u(5.0, 2.0, 1), t(0));
        s2.stale = true;
        let tot = totals([Some(&s1), Some(&s2), None]);
        assert_eq!(tot.balance_cu, 15.0);
        assert_eq!(tot.accounts, 2);
        assert!(!tot.complete);
        assert!(tot.any_stale);
    }

    #[test]
    fn unmanaged_count_stays_with_the_reading() {
        let mut a = AccountUsage::default();
        assert_eq!(a.unmanaged_runtimes(0), 0);
        a.record_success(u(80.0, 13.6, 3), t(0), &set(&["s1", "s2"]));
        assert_eq!(
            a.unmanaged_runtimes(1),
            1,
            "a stop does not change the reading"
        );
        assert_eq!(
            a.unmanaged_runtimes(3),
            1,
            "a start does not hide a runtime"
        );
        a.record_failure(Error::new(crate::ErrorKind::Http), t(30));
        assert_eq!(a.unmanaged_runtimes(3), 1);
        a.record_success(u(79.0, 13.6, 5), t(60), &set(&["s1", "s2", "s3"]));
        assert_eq!(
            a.unmanaged_runtimes(0),
            2,
            "a successful reading updates the count"
        );
        a.record_success(u(78.0, 13.6, 1), t(90), &set(&["s1", "s2"]));
        assert_eq!(a.unmanaged_runtimes(0), 0, "subtraction saturates");
    }

    #[test]
    fn older_usage_caches_fall_back_to_known_sessions() {
        let mut a = AccountUsage::default();
        a.record_success(u(80.0, 13.6, 3), t(0), &set(&["s1", "s2"]));
        let mut value = serde_json::to_value(&a).unwrap();
        value.as_object_mut().unwrap().remove("managed_at_reading");
        value.as_object_mut().unwrap().remove("ownership_verified");
        let legacy: AccountUsage = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.managed_at_reading, None);
        assert!(!legacy.ownership_verified);
        assert_eq!(legacy.unmanaged_runtimes(2), 1);
        let restored: AccountUsage =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(restored.unmanaged_runtimes(0), 1);
    }
}
