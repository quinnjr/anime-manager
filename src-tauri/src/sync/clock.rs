//! Hybrid logical clock: orders records written on different machines.
//! Fields compare in declaration order, so the derived `Ord` is the
//! last-writer-wins order, with the device id as the final tiebreak.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp {
    pub physical_ms: u64,
    pub counter: u32,
    pub device: String,
}

impl Stamp {
    pub const MIN_PHYSICAL: u64 = 0;

    pub fn min(device: &str) -> Stamp {
        Stamp { physical_ms: Self::MIN_PHYSICAL, counter: 0, device: device.to_string() }
    }

    /// `{physical:015}-{counter:010}-{device}`; string order equals `Ord` (ten digits
    /// hold any `u32`, so the counter never outgrows its padding).
    pub fn encode(&self) -> String {
        format!("{:015}-{:010}-{}", self.physical_ms, self.counter, self.device)
    }

    pub fn parse(s: &str) -> Option<Stamp> {
        // The device id is last and may itself contain dashes.
        let mut parts = s.splitn(3, '-');
        let physical_ms = parts.next()?.parse().ok()?;
        let counter = parts.next()?.parse().ok()?;
        let device = parts.next().filter(|d| !d.is_empty())?;
        Some(Stamp { physical_ms, counter, device: device.to_string() })
    }
}

/// Wall time in milliseconds since the Unix epoch (0 if the clock is before it).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Advance for a local event. `last` is the persisted clock, `now_ms` wall time.
pub fn tick(last: Option<&Stamp>, now_ms: u64, device: &str) -> Stamp {
    let floor = Stamp { physical_ms: now_ms, counter: 0, device: device.to_string() };
    advance(last, &floor, device, false)
}

/// Absorb a stamp seen in a remote file; returns the new persisted clock value.
pub fn observe(last: Option<&Stamp>, seen: &Stamp, device: &str) -> Stamp {
    advance(last, seen, device, true)
}

/// Fold `last` and `other` into a stamp strictly after `last` and, when
/// `after_other`, at or after `other` too. The counter continues from whichever
/// inputs share the winning physical time; it restarts at 0 only when `other`
/// alone supplies a newer wall time (a local tick).
fn advance(last: Option<&Stamp>, other: &Stamp, device: &str, after_other: bool) -> Stamp {
    let last_p = last.map_or(Stamp::MIN_PHYSICAL, |l| l.physical_ms);
    let physical_ms = last_p.max(other.physical_ms);
    let mut counter: Option<u32> = None;
    if let Some(l) = last.filter(|l| l.physical_ms == physical_ms) {
        counter = Some(l.counter);
    }
    if after_other && other.physical_ms == physical_ms {
        counter = Some(counter.map_or(other.counter, |c| c.max(other.counter)));
    }
    // A counter that cannot advance moves to the next millisecond instead, which still
    // sorts after every input.
    let (physical_ms, counter) = match counter {
        None => (physical_ms, 0),
        Some(u32::MAX) => (physical_ms + 1, 0),
        Some(c) => (physical_ms, c + 1),
    };
    Stamp { physical_ms, counter, device: device.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: u64, c: u32, d: &str) -> Stamp {
        Stamp { physical_ms: p, counter: c, device: d.into() }
    }

    #[test]
    fn encode_order_matches_ord() {
        let ordered = [s(5, 0, "a"), s(5, 1, "a"), s(5, 999_999, "a"), s(5, 1_000_000, "a"), s(5, u32::MAX, "a"),
            s(6, 0, "a"), s(6, 0, "b")];
        for w in ordered.windows(2) {
            assert!(w[0] < w[1]);
            assert!(w[0].encode() < w[1].encode());
        }
        for st in &ordered {
            assert_eq!(Stamp::parse(&st.encode()), Some(st.clone()));
        }
    }

    #[test]
    fn tick_is_monotonic_when_wall_clock_goes_back() {
        assert_eq!(tick(Some(&s(1000, 3, "d")), 500, "d"), s(1000, 4, "d"));
    }

    #[test]
    fn tick_resets_counter_when_time_advances() {
        assert_eq!(tick(Some(&s(1000, 3, "d")), 2000, "d"), s(2000, 0, "d"));
    }

    #[test]
    fn observe_then_tick_orders_after_remote() {
        let remote = s(9000, 7, "x");
        let clock = observe(Some(&s(1000, 0, "d")), &remote, "d");
        assert!(clock > remote);
        assert!(tick(Some(&clock), 1000, "d") > remote);
    }

    #[test]
    fn observe_without_a_clock_lands_after_the_seen_stamp() {
        let seen = s(9000, 7, "x");
        assert_eq!(observe(None, &seen, "d"), s(9000, 8, "d"));
    }

    #[test]
    fn observing_an_older_stamp_still_advances_past_the_clock() {
        let last = s(9000, 2, "d");
        let next = observe(Some(&last), &s(1000, 50, "x"), "d");
        assert_eq!(next, s(9000, 3, "d"));
        assert!(next > last);
    }

    #[test]
    fn observing_equal_physical_takes_the_larger_counter() {
        let next = observe(Some(&s(1000, 3, "d")), &s(1000, 7, "x"), "d");
        assert_eq!(next, s(1000, 8, "d"));
        assert_eq!(observe(Some(&s(1000, 9, "d")), &s(1000, 7, "x"), "d"), s(1000, 10, "d"));
    }

    #[test]
    fn a_full_counter_moves_to_the_next_millisecond() {
        let last = s(1000, u32::MAX, "d");
        assert_eq!(tick(Some(&last), 500, "d"), s(1001, 0, "d"));
        let seen = s(1000, u32::MAX, "x");
        let next = observe(Some(&s(1000, 1, "d")), &seen, "d");
        assert!(next > seen && next > last.clone());
        assert_eq!(next, s(1001, 0, "d"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(Stamp::parse("nope"), None);
    }
}
