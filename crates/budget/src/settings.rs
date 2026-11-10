//! The chart's memory settings, as a component's process reads them.

/// The container's memory limit in bytes, handed down by the chart from the
/// pod's own `limits.memory` (the downward API); read from the cgroup where
/// it is not set.
pub const LIMIT_ENV: &str = "MERIDIAN_MEMORY_LIMIT_BYTES";
/// `memory.budgetPercent`.
pub const BUDGET_PERCENT_ENV: &str = "MERIDIAN_MEMORY_BUDGET_PERCENT";
/// `memory.shedPercent`.
pub const SHED_PERCENT_ENV: &str = "MERIDIAN_MEMORY_SHED_PERCENT";

/// `memory.budgetPercent` where the chart says nothing (section 11).
pub const BUDGET_PERCENT: u32 = 80;
/// `memory.shedPercent` where the chart says nothing (section 11).
pub const SHED_PERCENT: u32 = 90;

/// The fewest and the most `memory.budgetPercent` may be: below 10 a
/// component can answer nothing, and the shed line must sit above it.
pub const BUDGET_PERCENT_RANGE: std::ops::RangeInclusive<u32> = 10..=95;
/// The most `memory.shedPercent` may be: at 100 the kernel's OOM killer
/// fires before the shed line is reached.
pub const SHED_PERCENT_MOST: u32 = 99;

/// A limit at or past this is no limit: what cgroup v1 says when none is set.
const NO_LIMIT_FROM: u64 = 1 << 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The container's limit; none where it has none (a desktop's compose).
    pub limit_bytes: Option<u64>,
    pub budget_percent: u32,
    pub shed_percent: u32,
}

impl Settings {
    /// No limit known: every reservation granted, nothing ever shed.
    pub fn unbounded() -> Self {
        Self {
            limit_bytes: None,
            budget_percent: BUDGET_PERCENT,
            shed_percent: SHED_PERCENT,
        }
    }

    /// The settings as given, the limit from `cgroup` where none is; each
    /// refused, naming its chart value, outside its range rather than read
    /// as something else.
    pub fn read(
        limit: Option<&str>,
        budget: Option<&str>,
        shed: Option<&str>,
        cgroup: impl FnOnce() -> Option<u64>,
    ) -> Result<Self, String> {
        let said = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let limit_bytes = match said(limit) {
            Some(text) => {
                let bytes = text.parse::<u64>().map_err(|_| {
                    format!("{LIMIT_ENV} is {text:?}; the container's memory limit in bytes")
                })?;
                (bytes > 0 && bytes < NO_LIMIT_FROM).then_some(bytes)
            }
            None => cgroup().filter(|bytes| *bytes > 0 && *bytes < NO_LIMIT_FROM),
        };
        let percent = |value: Option<String>, name: &str, default: u32| match value {
            None => Ok(default),
            Some(text) => text
                .parse::<u32>()
                .map_err(|_| format!("{name} is {text:?}; a whole number of percent")),
        };
        let budget_percent = percent(said(budget), "memory.budgetPercent", BUDGET_PERCENT)?;
        let shed_percent = percent(said(shed), "memory.shedPercent", SHED_PERCENT)?;
        if !BUDGET_PERCENT_RANGE.contains(&budget_percent) {
            return Err(format!(
                "memory.budgetPercent is {budget_percent}; a whole number from {} to {}",
                BUDGET_PERCENT_RANGE.start(),
                BUDGET_PERCENT_RANGE.end()
            ));
        }
        if shed_percent <= budget_percent || shed_percent > SHED_PERCENT_MOST {
            return Err(format!(
                "memory.shedPercent is {shed_percent}; above memory.budgetPercent ({budget_percent}) \
                 and at most {SHED_PERCENT_MOST}, so the largest request is cancelled before the \
                 kernel's OOM killer fires"
            ));
        }
        Ok(Self {
            limit_bytes,
            budget_percent,
            shed_percent,
        })
    }

    /// From this process's environment and, for the limit, its cgroup.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok();
        Self::read(
            var(LIMIT_ENV).as_deref(),
            var(BUDGET_PERCENT_ENV).as_deref(),
            var(SHED_PERCENT_ENV).as_deref(),
            cgroup_limit,
        )
    }

    /// The working budget: the limit times the budget percent, less `held`
    /// (the hot layer's bounds and the held answers' budget). None where no
    /// limit is known; refused where it leaves nothing.
    pub fn working_bytes(&self, held: u64) -> Result<Option<u64>, String> {
        let Some(limit) = self.limit_bytes else {
            return Ok(None);
        };
        let budget = limit / 100 * u64::from(self.budget_percent);
        match budget.checked_sub(held) {
            Some(working) if working >= crate::FLOOR_BYTES * 4 => Ok(Some(working)),
            _ => Err(format!(
                "a memory limit of {} MiB at memory.budgetPercent {} leaves {} MiB, and its hot \
                 layer and held answers take {} MiB: nothing is left to answer with; raise the \
                 limit or lower the bounds",
                limit / crate::MIB,
                self.budget_percent,
                budget / crate::MIB,
                held / crate::MIB
            )),
        }
    }

    /// The shed line: resident memory above it cancels the largest request.
    pub fn shed_bytes(&self) -> Option<u64> {
        self.limit_bytes
            .map(|limit| limit / 100 * u64::from(self.shed_percent))
    }
}

/// The container's limit as its cgroup says it: v2's `memory.max`, else
/// v1's `memory.limit_in_bytes`; none where neither is read or neither sets
/// one.
pub fn cgroup_limit() -> Option<u64> {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
    read("/sys/fs/cgroup/memory.max")
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn the_defaults_are_eighty_and_ninety_and_the_limit_comes_from_the_cgroup_where_unsaid() {
        let said = Settings::read(None, None, None, || Some(GIB)).unwrap();
        assert_eq!(said.budget_percent, 80);
        assert_eq!(said.shed_percent, 90);
        assert_eq!(said.limit_bytes, Some(GIB));
        let given =
            Settings::read(Some("536870912"), Some("70"), Some("85"), || Some(GIB)).unwrap();
        assert_eq!(given.limit_bytes, Some(GIB / 2));
        assert_eq!((given.budget_percent, given.shed_percent), (70, 85));
        // cgroup v1's "no limit", and v2's "max" (which does not parse).
        assert_eq!(
            Settings::read(None, None, None, || Some(u64::MAX / 2))
                .unwrap()
                .limit_bytes,
            None
        );
        assert_eq!(
            Settings::read(None, None, None, || None)
                .unwrap()
                .limit_bytes,
            None
        );
    }

    #[test]
    fn a_percent_out_of_range_or_a_shed_line_below_the_budget_is_refused_naming_it() {
        for (budget, shed, names) in [
            ("5", "90", "memory.budgetPercent"),
            ("96", "99", "memory.budgetPercent"),
            ("80", "80", "memory.shedPercent"),
            ("80", "100", "memory.shedPercent"),
            ("eighty", "90", "memory.budgetPercent"),
        ] {
            let said =
                Settings::read(Some("1073741824"), Some(budget), Some(shed), || None).unwrap_err();
            assert!(said.contains(names), "{budget}/{shed}: {said}");
        }
        assert!(Settings::read(Some("lots"), None, None, || None)
            .unwrap_err()
            .contains(LIMIT_ENV));
    }

    #[test]
    fn the_working_budget_is_the_limits_share_less_what_is_held_and_refused_where_nothing_is_left()
    {
        let settings = Settings::read(Some(&GIB.to_string()), None, None, || None).unwrap();
        // 1 GiB at 80 %, less 64 MiB of held answers and 100 MiB of hot layer.
        let working = settings.working_bytes(164 * 1024 * 1024).unwrap().unwrap();
        assert_eq!(working, GIB / 100 * 80 - 164 * 1024 * 1024);
        assert_eq!(settings.shed_bytes(), Some(GIB / 100 * 90));
        let said = settings.working_bytes(GIB).unwrap_err();
        assert!(said.contains("nothing is left"), "{said}");
        assert_eq!(Settings::unbounded().working_bytes(GIB), Ok(None));
    }
}
