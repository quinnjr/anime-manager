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

    /// `{physical:015}-{counter:06}-{device}`; string order equals `Ord`.
    pub fn encode(&self) -> String {
        format!("{:015}-{:06}-{}", self.physical_ms, self.counter, self.device)
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
    Stamp {
        physical_ms,
        counter: counter.map_or(0, |c| c + 1),
        device: device.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: u64, c: u32, d: &str) -> Stamp {
        Stamp { physical_ms: p, counter: c, device: d.into() }
    }

    #[test]
    fn encode_order_matches_ord() {
        let ordered = [s(5, 0, "a"), s(5, 1, "a"), s(6, 0, "a"), s(6, 0, "b")];
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
        assert!(clock >= remote);
        assert!(tick(Some(&clock), 1000, "d") > remote);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(Stamp::parse("nope"), None);
    }
}
