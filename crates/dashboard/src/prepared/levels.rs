//! What agents may do with each act (W6.29; the plan's Q4 to Q12, Q17, Q21):
//! the level -- **may make** (`may_make`), **prepare only**
//! (`prepare_only`), **may not** (`may_not`), each a value of `agents_may` --
//! and the act's second pair of eyes (`second_pair_of_eyes`).
//!
//! **Scopes.** The deployment admin sets core's acts (scope `act`) and a
//! ceiling on each plugin act (scope `ceiling`); a plugin's admin sets that
//! plugin's acts (scope `act`), within the ceiling; either may set a
//! stricter level for an access group (scope `group:<id>`). An act at no
//! scope is at may make with its gate off, but for the three acts on a
//! prepared change itself (`make_prepared`, `discard_prepared`,
//! `share_prepared`), at may not until an admin loosens them (Q17).
//!
//! **How they combine.** The level that applies to a call is the strictest
//! of the ceiling, the act's own, each access group's reaching the person,
//! and the person's own row (Prepare only, W6.17); the gate is on when any
//! of those scopes turns it on. A plugin admin's level above the ceiling is
//! kept, and shown capped: raising the ceiling again restores it. An act
//! that cannot be prepared takes only may make or may not, and no gate: a
//! prepare only reaching it, from an act later losing its check tool, reads
//! as may not, never as a way round.
//!
//! **No regulatory claims.** The words here say what each does -- an agent
//! may only prepare this; the maker is not whoever prepared it -- and
//! nothing more.

use std::collections::BTreeSet;

use super::store::LevelRow;

pub const CEILING: &str = "ceiling";
pub const ACT: &str = "act";
pub const GROUP: &str = "group:";

/// The upgrade's record (decisions/031).
pub const AT_THE_UPGRADE_SAID: &str = "may make, second pair of eyes off";
pub const AT_THE_UPGRADE_NOTE: &str = "at the upgrade to contract v20, for no person: every act \
     at may make and every second pair of eyes off, which is what the deployment allowed before";

/// The lapse period's name, its default and bounds (W6.28).
pub const LAPSE_DAYS: &str = "prepared_lapse_days";
pub const LAPSE_DEFAULT: u32 = 30;
pub const LAPSE_MOST: u32 = 365;

/// The three acts on a prepared change, at may not until loosened (Q17).
pub const ON_A_PREPARED_CHANGE: [&str; 3] = [
    "dashboard__make_prepared",
    "dashboard__discard_prepared",
    "dashboard__share_prepared",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    MayMake = 1,
    PrepareOnly = 2,
    MayNot = 3,
}

impl Level {
    pub fn from_code(code: i16) -> Level {
        match code {
            2 => Level::PrepareOnly,
            3 => Level::MayNot,
            _ => Level::MayMake,
        }
    }

    /// `agents_may`'s value.
    pub fn value(self) -> &'static str {
        match self {
            Level::MayMake => "may_make",
            Level::PrepareOnly => "prepare_only",
            Level::MayNot => "may_not",
        }
    }

    pub fn parse(said: &str) -> Option<Level> {
        match said {
            "may_make" => Some(Level::MayMake),
            "prepare_only" => Some(Level::PrepareOnly),
            "may_not" => Some(Level::MayNot),
            _ => None,
        }
    }

    /// As the pages and a refusal say it.
    pub fn words(self) -> &'static str {
        match self {
            Level::MayMake => "May make",
            Level::PrepareOnly => "Prepare only",
            Level::MayNot => "May not",
        }
    }
}

/// The level an act stands at with no row of its own.
pub fn default_for(act: &str) -> Level {
    if ON_A_PREPARED_CHANGE.contains(&act) {
        Level::MayNot
    } else {
        Level::MayMake
    }
}

/// A level and a gate, as a record says them: `prepare_only, second pair of
/// eyes on`.
pub fn said(level: Level, eyes: bool) -> String {
    format!(
        "{}, second pair of eyes {}",
        level.value(),
        if eyes { "on" } else { "off" }
    )
}

/// What applies to one call, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applies {
    pub level: Level,
    pub second_pair_of_eyes: bool,
    /// Where the level came from: `the ceiling`, `the act`, `access group
    /// Traders`, `this delegation's row`.
    pub from: String,
}

/// The levels as they stand, read once.
#[derive(Debug, Clone, Default)]
pub struct Levels {
    pub rows: Vec<LevelRow>,
}

impl Levels {
    pub fn at(&self, act: &str, scope: &str) -> Option<&LevelRow> {
        self.rows
            .iter()
            .find(|row| row.act == act && row.scope == scope)
    }

    /// The act's own level, its admins' (or its default), and its gate.
    pub fn own(&self, act: &str) -> (Level, bool) {
        match self.at(act, ACT) {
            Some(row) => (Level::from_code(row.level), row.second_pair_of_eyes),
            None => (default_for(act), false),
        }
    }

    /// The ceiling on an act, where the deployment admin set one.
    pub fn ceiling(&self, act: &str) -> Option<(Level, bool)> {
        self.at(act, CEILING)
            .map(|row| (Level::from_code(row.level), row.second_pair_of_eyes))
    }

    /// The level and gate that apply to a call on `act` by a person whose
    /// access groups are `groups`, through a delegation whose own row for it
    /// is Prepare only where `prepare_only_row`; `preparable` whether the act
    /// names a check (core's thirteen, or a plugin act with `checked_by`).
    pub fn applies(
        &self,
        act: &str,
        groups: &BTreeSet<String>,
        preparable: bool,
        prepare_only_row: bool,
    ) -> Applies {
        let (own, own_eyes) = self.own(act);
        let mut level = own;
        let mut eyes = own_eyes;
        let mut from = "the act".to_string();
        if let Some((ceiling, ceiling_eyes)) = self.ceiling(act) {
            eyes |= ceiling_eyes;
            if ceiling > level {
                level = ceiling;
                from = "the ceiling".into();
            }
        }
        for group in groups {
            if let Some(row) = self.at(act, &format!("{GROUP}{group}")) {
                eyes |= row.second_pair_of_eyes;
                let stricter = Level::from_code(row.level);
                if stricter > level {
                    level = stricter;
                    from = format!("access group {group}");
                }
            }
        }
        if prepare_only_row && level < Level::PrepareOnly {
            level = Level::PrepareOnly;
            from = "this delegation's row".into();
        }
        if !preparable {
            eyes = false;
            if level == Level::PrepareOnly {
                level = Level::MayNot;
            }
        }
        Applies {
            level,
            second_pair_of_eyes: eyes,
            from,
        }
    }

    /// The act's own level as an admin chose it and as it stands under the
    /// ceiling: (chosen, standing, capped).
    pub fn capped(&self, act: &str) -> (Level, Level, bool) {
        let (own, _) = self.own(act);
        match self.ceiling(act) {
            Some((ceiling, _)) if ceiling > own => (own, ceiling, true),
            _ => (own, own, false),
        }
    }
}

/// Why a level may not be set at a scope, or None: prepare only, or a gate,
/// on an act that cannot be prepared; the gate on at the ceiling is never
/// turned off below it (that one is kept and shown).
pub fn refused(level: Level, eyes: bool, preparable: bool) -> Option<String> {
    if !preparable && level == Level::PrepareOnly {
        return Some(
            "level: this act names no check, so it cannot be prepared: agents may make it or may \
             not reach it"
                .into(),
        );
    }
    if !preparable && eyes {
        return Some(
            "second_pair_of_eyes: this act names no check, so it cannot be prepared, and a \
             second pair of eyes needs a prepared change to make"
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(act: &str, scope: &str, level: Level, eyes: bool) -> LevelRow {
        LevelRow {
            act: act.into(),
            scope: scope.into(),
            level: level as i16,
            second_pair_of_eyes: eyes,
        }
    }

    const ACT_NAME: &str = "ops-1__confirm_opening_balance";

    #[test]
    fn the_strictest_applies_and_a_capped_level_is_kept() {
        let none = BTreeSet::new();
        let mut levels = Levels::default();
        assert_eq!(
            levels.applies(ACT_NAME, &none, true, false).level,
            Level::MayMake
        );
        levels.rows.push(row(ACT_NAME, ACT, Level::MayMake, false));
        levels
            .rows
            .push(row(ACT_NAME, CEILING, Level::PrepareOnly, false));
        let applies = levels.applies(ACT_NAME, &none, true, false);
        assert_eq!(applies.level, Level::PrepareOnly);
        assert_eq!(applies.from, "the ceiling");
        assert_eq!(
            levels.capped(ACT_NAME),
            (Level::MayMake, Level::PrepareOnly, true)
        );
        // Raised again, the plugin admin's choice stands.
        levels.rows.retain(|r| r.scope != CEILING);
        assert_eq!(
            levels.applies(ACT_NAME, &none, true, false).level,
            Level::MayMake
        );
    }

    #[test]
    fn an_access_groups_level_binds_its_members_and_no_one_else() {
        let mut levels = Levels::default();
        levels
            .rows
            .push(row(ACT_NAME, "group:AG-TRADERS", Level::MayNot, false));
        let traders: BTreeSet<String> = ["AG-TRADERS".to_string()].into();
        assert_eq!(
            levels.applies(ACT_NAME, &traders, true, false).level,
            Level::MayNot
        );
        assert_eq!(
            levels
                .applies(ACT_NAME, &BTreeSet::new(), true, false)
                .level,
            Level::MayMake
        );
    }

    #[test]
    fn a_persons_row_is_only_stricter() {
        let mut levels = Levels::default();
        let none = BTreeSet::new();
        assert_eq!(
            levels.applies(ACT_NAME, &none, true, true).level,
            Level::PrepareOnly
        );
        levels.rows.push(row(ACT_NAME, ACT, Level::MayNot, false));
        assert_eq!(
            levels.applies(ACT_NAME, &none, true, true).level,
            Level::MayNot
        );
    }

    #[test]
    fn the_gate_is_on_when_any_scope_turns_it_on_and_never_on_an_unpreparable_act() {
        let mut levels = Levels::default();
        let none = BTreeSet::new();
        levels
            .rows
            .push(row(ACT_NAME, CEILING, Level::MayMake, true));
        levels.rows.push(row(ACT_NAME, ACT, Level::MayMake, false));
        assert!(
            levels
                .applies(ACT_NAME, &none, true, false)
                .second_pair_of_eyes
        );
        let unpreparable = levels.applies(ACT_NAME, &none, false, true);
        assert!(!unpreparable.second_pair_of_eyes);
        assert_eq!(unpreparable.level, Level::MayNot);
    }

    #[test]
    fn the_acts_on_a_prepared_change_start_at_may_not() {
        let levels = Levels::default();
        for act in ON_A_PREPARED_CHANGE {
            assert_eq!(levels.own(act).0, Level::MayNot);
        }
        assert_eq!(levels.own("dashboard__set_hold").0, Level::MayMake);
    }

    #[test]
    fn prepare_only_and_the_gate_are_refused_on_an_act_that_cannot_be_prepared() {
        assert!(refused(Level::PrepareOnly, false, false).is_some());
        assert!(refused(Level::MayMake, true, false).is_some());
        assert!(refused(Level::MayNot, false, false).is_none());
        assert!(refused(Level::PrepareOnly, true, true).is_none());
    }
}
