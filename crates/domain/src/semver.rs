//! SemVer 2.0 for listed versions (W8.10, W8.11; the registries spec's
//! requirement 8), which the conductor offers updates by and the dashboard
//! shows them by, alike.

/// SemVer 2.0 precedence (requirement 8): major, minor and patch by
/// number, a pre-release below its release, its identifiers compared
/// numerically where both are numbers; build metadata ignored. None for a
/// version that is not SemVer.
pub fn semver_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parse(v: &str) -> Option<([u64; 3], Vec<String>)> {
        let v = v.split('+').next()?;
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').map(String::from).collect()),
            None => (v, Vec::new()),
        };
        let mut parts = core.split('.');
        let mut out = [0u64; 3];
        for slot in &mut out {
            *slot = parts.next()?.parse().ok()?;
        }
        parts.next().is_none().then_some((out, pre))
    }
    let (a_core, a_pre) = parse(a)?;
    let (b_core, b_pre) = parse(b)?;
    use std::cmp::Ordering::*;
    Some(
        a_core
            .cmp(&b_core)
            .then_with(|| match (a_pre.is_empty(), b_pre.is_empty()) {
                (true, true) => Equal,
                (true, false) => Greater,
                (false, true) => Less,
                (false, false) => {
                    for (x, y) in a_pre.iter().zip(&b_pre) {
                        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Less,
                            (Err(_), Ok(_)) => Greater,
                            (Err(_), Err(_)) => x.cmp(y),
                        };
                        if order != Equal {
                            return order;
                        }
                    }
                    a_pre.len().cmp(&b_pre.len())
                }
            }),
    )
}

pub fn is_prerelease(version: &str) -> bool {
    version.split('+').next().is_some_and(|v| v.contains('-'))
}
