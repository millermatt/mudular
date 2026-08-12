//! What a character's own numbers say about them, read out of the merged
//! server-data store (docs/ARCHITECTURE.md §6.3, §11.6).
//!
//! A dependency-free leaf like [`crate::map`], and for the same reasons: it
//! takes an already-flattened `HashMap` and does no I/O, so it stays inside
//! §4's boundary rule, and it reads the *merged* store rather than one
//! protocol's own event — a server that switched from GMCP to MSDP without
//! changing its numbers would otherwise take the party strip down with it.
//!
//! Tolerant by design. Every gauge is optional, since a MUD that reports
//! health and nothing else should still show health; a gauge with no
//! maximum is no gauge, because a bar needs both ends.

use std::collections::HashMap;

/// One number with a ceiling — health, mana, whatever the MUD calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauge {
    pub now: i64,
    pub max: i64,
}

impl Gauge {
    /// How full, clamped to 0..=1. A server that reports more than the
    /// maximum (a temporary buff, a stale max) must not draw past the end
    /// of its own bar.
    pub fn fraction(&self) -> f64 {
        if self.max <= 0 {
            return 0.0;
        }
        (self.now.max(0) as f64 / self.max as f64).min(1.0)
    }
}

/// A character's gauges, as far as the server has said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Vitals {
    pub health: Option<Gauge>,
    pub mana: Option<Gauge>,
    pub movement: Option<Gauge>,
}

impl Vitals {
    /// True when the server has said nothing worth drawing.
    pub fn is_empty(&self) -> bool {
        self.health.is_none() && self.mana.is_none() && self.movement.is_none()
    }

    /// How much trouble this character is in, or `None` for none — the
    /// number the party strip's red already means, named so the rest of the
    /// client can act on it (§11.7).
    ///
    /// **Health alone.** Mana at a tenth is a caster out of spells and
    /// movement at a tenth is a long walk home; neither is a character
    /// about to die, and an alarm that fires for all three is an alarm
    /// nobody looks up for. A server that reports no health reports no
    /// trouble, on §11.6's "both ends or nothing" rule — a missing gauge is
    /// silence, not zero.
    pub fn distress(&self) -> Option<f64> {
        let health = self.health?;
        let filled = health.fraction();
        (filled <= ALARM).then_some(filled)
    }
}

/// How empty a gauge has to be before the client says something about it.
///
/// One constant rather than one per caller: the strip paints this red and
/// "who needs me?" jumps to it, and a client whose alarm colour and alarm
/// key disagreed about what counts as trouble would teach the player to
/// trust neither.
pub const ALARM: f64 = 0.25;

/// Where each protocol puts a gauge, **GMCP first** for the same reason
/// §16's room table is: §6.3's store-level precedence settles keys that
/// collide *exactly*, and `Char.Vitals.hp` and `HEALTH` are different keys
/// carrying the same fact, so the order of the lookup is where that
/// precedence actually lives.
///
/// Spellings vary more than the protocols do — IRE says `hp`/`maxhp`,
/// Aardwolf says `hp` but spells movement `moves` — so each row lists what
/// servers are actually seen to send rather than what a spec suggests.
const HEALTH: (&[&str], &[&str]) = (
    &["Char.Vitals.hp", "HEALTH"],
    &["Char.Vitals.maxhp", "HEALTH_MAX"],
);
const MANA: (&[&str], &[&str]) = (
    &["Char.Vitals.mp", "MANA"],
    &["Char.Vitals.maxmp", "MANA_MAX"],
);
const MOVEMENT: (&[&str], &[&str]) = (
    &["Char.Vitals.mv", "Char.Vitals.moves", "MOVEMENT"],
    &["Char.Vitals.maxmv", "Char.Vitals.maxmoves", "MOVEMENT_MAX"],
);

/// Reads the gauges a server has reported, or an empty set if it has
/// reported none.
pub fn from_server_data(data: &HashMap<String, String>) -> Vitals {
    Vitals {
        health: gauge(data, HEALTH),
        mana: gauge(data, MANA),
        movement: gauge(data, MOVEMENT),
    }
}

fn gauge(data: &HashMap<String, String>, (now, max): (&[&str], &[&str])) -> Option<Gauge> {
    // Both ends or nothing: a current value with no ceiling cannot be drawn
    // as a bar, and guessing one would be inventing the number that decides
    // whether a character looks safe.
    Some(Gauge {
        now: number(data, now)?,
        max: number(data, max)?,
    })
}

fn number(data: &HashMap<String, String>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| lookup_ci(data, key)?.parse().ok())
}

/// Case-insensitive lookup in a case-sensitive store, resolved the same way
/// every time. Several stored keys can match one name, and letting
/// `HashMap` order pick between them makes the same store answer
/// differently between runs — see `map::lookup_ci`, which learned this the
/// hard way.
fn lookup_ci<'a>(data: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    data.iter()
        .filter(|(stored, _)| stored.eq_ignore_ascii_case(key))
        .min_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn reads_gmcp_vitals() {
        let vitals = from_server_data(&store(&[
            ("Char.Vitals.hp", "72"),
            ("Char.Vitals.maxhp", "100"),
            ("Char.Vitals.mp", "10"),
            ("Char.Vitals.maxmp", "40"),
        ]));

        assert_eq!(vitals.health, Some(Gauge { now: 72, max: 100 }));
        assert_eq!(vitals.mana, Some(Gauge { now: 10, max: 40 }));
        assert_eq!(vitals.movement, None);
    }

    /// The MSDP spelling a DikuMUD-derived server sends — the only one the
    /// live test target speaks.
    #[test]
    fn reads_msdp_vitals() {
        let vitals = from_server_data(&store(&[
            ("HEALTH", "27"),
            ("HEALTH_MAX", "30"),
            ("MOVEMENT", "80"),
            ("MOVEMENT_MAX", "83"),
        ]));

        assert_eq!(vitals.health, Some(Gauge { now: 27, max: 30 }));
        assert_eq!(vitals.movement, Some(Gauge { now: 80, max: 83 }));
    }

    /// §6.3's precedence, applied to keys that carry the same fact under
    /// different names: a server speaking both is read as GMCP.
    #[test]
    fn gmcp_wins_over_msdp_for_the_same_gauge() {
        let vitals = from_server_data(&store(&[
            ("Char.Vitals.hp", "72"),
            ("Char.Vitals.maxhp", "100"),
            ("HEALTH", "5"),
            ("HEALTH_MAX", "100"),
        ]));

        assert_eq!(vitals.health, Some(Gauge { now: 72, max: 100 }));
    }

    /// A bar needs both ends. Half a gauge is not a smaller gauge, it is a
    /// number whose meaning nobody knows.
    #[test]
    fn a_gauge_with_no_maximum_is_not_shown() {
        let vitals = from_server_data(&store(&[("HEALTH", "27")]));

        assert!(vitals.health.is_none());
        assert!(vitals.is_empty());
    }

    /// Servers overshoot their own maxima — a buff, or a stale ceiling —
    /// and a bar must not draw past its end.
    #[test]
    fn a_gauge_over_its_maximum_is_full_rather_than_more() {
        let over = Gauge { now: 120, max: 100 };
        assert_eq!(over.fraction(), 1.0);

        let negative = Gauge { now: -8, max: 100 };
        assert_eq!(negative.fraction(), 0.0);

        let nonsense = Gauge { now: 5, max: 0 };
        assert_eq!(nonsense.fraction(), 0.0);
    }

    #[test]
    fn a_server_that_says_nothing_shows_nothing() {
        assert!(from_server_data(&store(&[("Room.Info.num", "1")])).is_empty());
    }

    #[test]
    fn trouble_is_a_quarter_of_health_or_less() {
        let hurt = from_server_data(&store(&[("HEALTH", "25"), ("HEALTH_MAX", "100")]));
        assert_eq!(hurt.distress(), Some(0.25));

        let fine = from_server_data(&store(&[("HEALTH", "26"), ("HEALTH_MAX", "100")]));
        assert_eq!(fine.distress(), None);
    }

    /// A caster out of spells is not a character about to die, and an alarm
    /// that fires for both is one nobody looks up for.
    #[test]
    fn an_empty_mana_bar_is_not_trouble() {
        let dry = from_server_data(&store(&[
            ("HEALTH", "100"),
            ("HEALTH_MAX", "100"),
            ("MANA", "1"),
            ("MANA_MAX", "100"),
            ("MOVEMENT", "1"),
            ("MOVEMENT_MAX", "100"),
        ]));

        assert_eq!(dry.distress(), None);
    }

    /// A missing gauge is silence, not zero — §11.6's rule, which the alarm
    /// has to keep or every MUD that reports nothing would look like a
    /// party of corpses.
    #[test]
    fn a_character_with_no_health_reported_is_not_in_trouble() {
        assert_eq!(from_server_data(&store(&[("MANA", "1")])).distress(), None);
    }
}
