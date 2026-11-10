//! The last line, and memory handed back (section 10a).
//!
//! Each component's sweep reads its resident memory. Above
//! `memory.shedPercent` of its limit it cancels its largest request in
//! flight with the overload refusal, so the kernel's OOM killer never fires;
//! and the memory freed is returned to the system with glibc's
//! `malloc_trim`, under the arena and mmap-threshold settings the chart
//! carries to every component (`<component>.mallocArenaMax`,
//! `<component>.mallocMmapThreshold`). No allocator change (959).

use std::time::Duration;

use crate::{Process, MIB};

/// How often resident memory is read against the shed line.
pub const EVERY: Duration = Duration::from_millis(500);

/// How often the memory freed is handed back while nothing was shed.
pub const TRIM_EVERY: Duration = Duration::from_secs(30);

/// The process's resident memory in bytes, as /proc says it; none off Linux.
pub fn resident_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

/// Ruling 957 (b): glibc's free memory handed back to the system, from the
/// top of each arena and the free pages within it; what it did said at
/// debug. Only glibc keeps memory this way, and only glibc is asked.
#[cfg(target_env = "gnu")]
pub fn release_to_system() {
    let told = tracing::enabled!(tracing::Level::DEBUG);
    let before = told.then(resident_bytes).flatten();
    let started = std::time::Instant::now();
    // SAFETY: malloc_trim takes no pointer and touches only the allocator's
    // own state, under the allocator's own locks.
    let released = unsafe { libc::malloc_trim(0) } == 1;
    if told {
        tracing::debug!(
            released,
            resident_before_kib = before.unwrap_or(0) / 1024,
            resident_after_kib = resident_bytes().unwrap_or(0) / 1024,
            ms = format!("{:.1}", started.elapsed().as_secs_f64() * 1e3),
            "the memory freed returned to the system"
        );
    }
}

#[cfg(not(target_env = "gnu"))]
pub fn release_to_system() {}

/// One look: where `resident` is above the shed line, the largest request in
/// flight cancelled and said; what was cancelled, if anything.
pub fn shed_if_over(process: &Process, resident: u64) -> Option<crate::Shed> {
    process.saw_resident(resident);
    let line = process.settings.shed_bytes()?;
    if resident <= line {
        return None;
    }
    let shed = process.budget.shed_largest();
    match &shed {
        Some(shed) => tracing::warn!(
            component = process.component,
            resident_mib = resident / MIB,
            shed_line_mib = line / MIB,
            cancelled = shed.what,
            cancelled_mib = shed.bytes.div_ceil(MIB),
            "above its shed line: its largest request in flight is refused, never the component"
        ),
        None => tracing::warn!(
            component = process.component,
            resident_mib = resident / MIB,
            shed_line_mib = line / MIB,
            "above its shed line with no request in flight to cancel; the memory freed is handed back"
        ),
    }
    shed
}

/// The sweep, for as long as the process runs.
pub async fn forever(process: &'static Process) {
    let mut since_trim = Duration::ZERO;
    loop {
        tokio::time::sleep(EVERY).await;
        since_trim += EVERY;
        let over = resident_bytes().map(|resident| (resident, shed_if_over(process, resident)));
        let above = matches!(over, Some((resident, _)) if process.settings.shed_bytes().is_some_and(|line| resident > line));
        if above || since_trim >= TRIM_EVERY {
            since_trim = Duration::ZERO;
            // On the blocking pool: trimming a large heap takes milliseconds.
            let _ = tokio::task::spawn_blocking(release_to_system).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, MIB};

    #[test]
    fn above_the_shed_line_the_largest_request_is_cancelled_and_below_it_nothing() {
        let settings = Settings::read(
            Some(&(100 * MIB).to_string()),
            Some("80"),
            Some("90"),
            || None,
        )
        .unwrap();
        let process = Process::new("lake", settings, 0, 4).unwrap();
        let small = process
            .budget
            .reserve(MIB, "small", Duration::ZERO)
            .unwrap();
        let large = process
            .budget
            .reserve(20 * MIB, "large", Duration::ZERO)
            .unwrap();
        assert_eq!(shed_if_over(&process, 89 * MIB), None);
        assert!(!large.cancelled());
        let shed = shed_if_over(&process, 95 * MIB).unwrap();
        assert_eq!(shed.what, "large");
        assert!(large.cancelled() && !small.cancelled());
    }

    #[test]
    fn what_the_budget_did_is_read_in_one_place_for_the_components_report() {
        let settings = Settings::read(
            Some(&(100 * MIB).to_string()),
            Some("80"),
            Some("90"),
            || None,
        )
        .unwrap();
        let process = Process::new("bor", settings, 0, 4).unwrap();
        let held = process
            .budget
            .reserve(30 * MIB, "held", Duration::ZERO)
            .unwrap();
        assert!(process
            .budget
            .reserve(60 * MIB, "refused", Duration::ZERO)
            .is_err());
        assert!(shed_if_over(&process, 95 * MIB).is_some());
        drop(held);
        let said = process.accounting();
        assert_eq!(said.limit, Some(100 * MIB));
        assert_eq!(said.working, Some(100 * MIB / 100 * 80));
        assert_eq!((said.reserved, said.peak_reserved), (0, 30 * MIB));
        assert_eq!((said.granted, said.refused, said.shed), (1, 1, 1));
        assert!(said.peak_resident >= 95 * MIB);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_memory_is_read() {
        assert!(resident_bytes().unwrap() > 0);
    }
}
