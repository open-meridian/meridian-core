//! The catalogue, one page at a time (W8.2; contract v28's amendment, ruling
//! 696).
//!
//! The answer grew with every version uploaded -- each with its declaration
//! -- and every launch ever made, and on dev, 2026-10-26, it reached
//! 1,050,251 bytes, past the broker's 1 MiB: every read of it waited out its
//! timeout. Now a page ends at [`PAGE_BUDGET`], whatever the request asks,
//! and says where the next one starts. A request naming nothing is answered
//! as before, up to that budget.
//!
//! The order is the versions by name and version, then the launches in the
//! order they were made. A cursor names the last version a page held, or how
//! many launches the pages before held: launches are only ever added after
//! the last, never removed or reordered, so a version uploaded or a launch
//! made between two pages is neither repeated nor skipped behind the
//! cursor.

use meridian_bus::{refusal_with_status, RefusalStatus};
use meridian_domain::v1::{PluginCatalogue, PluginCatalogueRequest, PluginLaunch, PluginVersion};
use prost::Message;

/// About 512 KiB: half the broker's 1 MiB, so a page and its envelope fit
/// with room to spare (ruling 696).
pub(crate) const PAGE_BUDGET: usize = 512 * 1024;

/// The most records a page holds where a request names a page size, as
/// every paged read caps it.
const MOST_PAGE_SIZE: usize = 500;

/// Room kept for next_cursor and the envelope around the page.
const KEPT_FOR_THE_REST: usize = 4 * 1024;

const VERSION_MARK: &str = "v";
const LAUNCH_MARK: &str = "l";
/// Between a cursor's parts: no plugin's name (letters, digits, hyphens)
/// or SemVer version holds one.
const PART: char = '/';

/// Where a page starts: after a version, or after so many launches (every
/// version then behind it).
#[derive(Debug, PartialEq)]
enum After {
    Nothing,
    Version(String, String),
    Launches(usize),
}

fn version_key(held: &PluginVersion) -> (String, String) {
    let metadata = held.metadata.clone().unwrap_or_default();
    (metadata.name, metadata.version)
}

fn cursor_after_version(held: &PluginVersion) -> String {
    let (name, version) = version_key(held);
    format!("{VERSION_MARK}{PART}{name}{PART}{version}")
}

fn cursor_after_launches(given: usize) -> String {
    format!("{LAUNCH_MARK}{PART}{given}{PART}")
}

fn invalid(field: &str, words: &str) -> String {
    refusal_with_status(RefusalStatus::InvalidArgument, &[field.to_string()], words)
}

fn read_cursor(cursor: &str) -> Result<After, String> {
    if cursor.is_empty() {
        return Ok(After::Nothing);
    }
    let not_ours = || {
        invalid(
            "cursor",
            "not a cursor this catalogue gave: start again without one",
        )
    };
    let mut parts = cursor.splitn(3, PART);
    let (Some(mark), Some(first), Some(second)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(not_ours());
    };
    match mark {
        VERSION_MARK => Ok(After::Version(first.to_string(), second.to_string())),
        LAUNCH_MARK => first
            .parse::<usize>()
            .ok()
            .filter(|_| second.is_empty())
            .map(After::Launches)
            .ok_or_else(not_ours),
        _ => Err(not_ours()),
    }
}

/// The bytes a record adds to the page: its tag, its length and itself.
fn added(len: usize) -> usize {
    1 + prost::encoding::encoded_len_varint(len as u64) + len
}

/// One page of `held` -- every version not retired and every launch -- as
/// `request` asks for it.
pub(crate) fn page(
    held: PluginCatalogue,
    request: &PluginCatalogueRequest,
) -> Result<PluginCatalogue, String> {
    page_within(held, request, PAGE_BUDGET)
}

/// [`page`], within `budget` bytes: the tests' own budget is smaller.
pub(crate) fn page_within(
    held: PluginCatalogue,
    request: &PluginCatalogueRequest,
    budget: usize,
) -> Result<PluginCatalogue, String> {
    if request.page_size < 0 {
        return Err(invalid(
            "page_size",
            "from 0, where 0 is as many as a page holds",
        ));
    }
    if !request.version.is_empty() && request.name.is_empty() {
        return Err(invalid(
            "version",
            "a version is read with its plugin's name",
        ));
    }
    let after = read_cursor(&request.cursor)?;

    let named = |name: &str, version: &str| {
        (request.name.is_empty() || request.name == name)
            && (request.version.is_empty() || request.version == version)
    };

    let mut versions: Vec<PluginVersion> = if request.launches_only {
        Vec::new()
    } else {
        held.versions
            .into_iter()
            .filter(|v| {
                let (name, version) = version_key(v);
                named(&name, &version)
            })
            .collect()
    };
    versions.sort_by_key(version_key);
    let mut launches: Vec<PluginLaunch> = held
        .launches
        .into_iter()
        .filter(|l| named(&l.name, &l.version))
        .collect();

    let given_before = match &after {
        After::Launches(given) => *given,
        _ => 0,
    };
    match &after {
        After::Nothing => {}
        After::Version(name, version) => {
            let at = (name.clone(), version.clone());
            versions.retain(|v| version_key(v) > at);
        }
        After::Launches(given) => {
            versions.clear();
            launches.drain(..(*given).min(launches.len()));
        }
    }

    // 0 is as many as the budget takes; past 500, answered at 500.
    let most = match request.page_size {
        0 => usize::MAX,
        n => (n as usize).min(MOST_PAGE_SIZE),
    };
    let room = budget.saturating_sub(KEPT_FOR_THE_REST);
    let mut answer = PluginCatalogue::default();
    let mut used = 0usize;
    let mut taken = 0usize;
    // A page holds at least one record, whatever its size, so a page always
    // moves the reader on.
    let fits = |used: usize, more: usize, taken: usize| taken == 0 || used + more <= room;

    let mut rest_versions = versions.into_iter().peekable();
    let mut full = false;
    while let Some(next) = rest_versions.peek() {
        let more = added(next.encoded_len());
        if taken >= most || !fits(used, more, taken) {
            full = true;
            break;
        }
        used += more;
        taken += 1;
        answer.versions.push(rest_versions.next().expect("peeked"));
    }
    let mut rest_launches = launches.into_iter().peekable();
    if !full {
        while let Some(next) = rest_launches.peek() {
            let more = added(next.encoded_len());
            if taken >= most || !fits(used, more, taken) {
                full = true;
                break;
            }
            used += more;
            taken += 1;
            answer.launches.push(rest_launches.next().expect("peeked"));
        }
    }

    if full {
        answer.next_cursor = match (answer.launches.last(), answer.versions.last()) {
            (Some(_), _) => cursor_after_launches(given_before + answer.launches.len()),
            (None, Some(version)) => cursor_after_version(version),
            (None, None) => unreachable!("a page holds at least one record"),
        };
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{PluginLaunchState, PluginMetadata};
    use meridian_pb::v1::{PluginDeclaration, ToolDeclaration};

    fn version(name: &str, version: &str, words: usize) -> PluginVersion {
        PluginVersion {
            metadata: Some(PluginMetadata {
                name: name.into(),
                version: version.into(),
                declaration: Some(PluginDeclaration {
                    tools: vec![ToolDeclaration {
                        name: "read_figures".into(),
                        description: "what it reads, as the vendor documents it. ".repeat(words),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            image_digest: format!("sha256:{name}-{version}"),
            ..Default::default()
        }
    }

    fn launch(instance: &str, name: &str, version: &str, at: i64) -> PluginLaunch {
        PluginLaunch {
            instance_id: instance.into(),
            name: name.into(),
            version: version.into(),
            launched_at_ns: at,
            state: PluginLaunchState::Launched as i32,
            ..Default::default()
        }
    }

    /// Dev's catalogue on 2026-10-26, near enough: 22 plugins, many
    /// versions, each declaring tools documented at length -- past 1 MiB.
    fn past_a_mebibyte() -> PluginCatalogue {
        let mut held = PluginCatalogue::default();
        for n in 0..22 {
            for v in 0..12 {
                held.versions
                    .push(version(&format!("plugin-{n:02}"), &format!("0.{v}.0"), 100));
            }
            held.launches.push(launch(
                &format!("plugin-{n:02}"),
                &format!("plugin-{n:02}"),
                "0.11.0",
                n,
            ));
        }
        assert!(
            held.encoded_len() > 1_048_576,
            "{} bytes",
            held.encoded_len()
        );
        held
    }

    fn every_page(
        held: &PluginCatalogue,
        mut request: PluginCatalogueRequest,
    ) -> Vec<PluginCatalogue> {
        let mut pages = Vec::new();
        loop {
            let answer = page(held.clone(), &request).expect("a page");
            let next = answer.next_cursor.clone();
            pages.push(answer);
            if next.is_empty() {
                return pages;
            }
            assert!(pages.len() < 1000, "the pages never end");
            request.cursor = next;
        }
    }

    #[test]
    fn a_catalogue_past_a_mebibyte_is_answered_in_pages_each_within_the_budget() {
        let held = past_a_mebibyte();
        let pages = every_page(&held, PluginCatalogueRequest::default());
        assert!(pages.len() > 2, "{} pages", pages.len());
        for answer in &pages {
            assert!(
                answer.encoded_len() <= PAGE_BUDGET,
                "{} bytes",
                answer.encoded_len()
            );
        }
        // Every record once, in order: the versions by name and version,
        // then the launches as they were made.
        let versions: Vec<_> = pages
            .iter()
            .flat_map(|p| p.versions.iter().map(version_key))
            .collect();
        let mut expected: Vec<_> = held.versions.iter().map(version_key).collect();
        expected.sort();
        assert_eq!(versions, expected);
        let launches: Vec<_> = pages
            .iter()
            .flat_map(|p| p.launches.iter().map(|l| l.launched_at_ns))
            .collect();
        assert_eq!(launches, (0..22).collect::<Vec<i64>>(), "as they were made");
        assert!(pages.last().unwrap().next_cursor.is_empty());
    }

    #[test]
    fn an_empty_request_is_answered_whole_where_it_fits() {
        let mut held = PluginCatalogue::default();
        held.versions.push(version("kraken", "0.10.0", 2));
        held.versions.push(version("alpaca", "0.3.0", 2));
        held.launches.push(launch("kraken", "kraken", "0.10.0", 5));
        let answer = page(held.clone(), &PluginCatalogueRequest::default()).unwrap();
        assert_eq!(answer.versions.len(), 2);
        assert_eq!(answer.launches.len(), 1);
        assert!(answer.next_cursor.is_empty(), "one page, as before");
        // By name, then version.
        assert_eq!(version_key(&answer.versions[0]).0, "alpaca");
    }

    #[test]
    fn launches_only_answers_no_version() {
        let held = past_a_mebibyte();
        let pages = every_page(
            &held,
            PluginCatalogueRequest {
                launches_only: true,
                ..Default::default()
            },
        );
        assert_eq!(pages.len(), 1, "the launches alone fit one page");
        assert!(pages[0].versions.is_empty());
        assert_eq!(pages[0].launches.len(), 22);
        assert!(pages[0].encoded_len() < 16 * 1024);
    }

    #[test]
    fn a_name_and_version_narrow_to_that_version_and_its_launches() {
        let held = past_a_mebibyte();
        let answer = page(
            held.clone(),
            &PluginCatalogueRequest {
                name: "plugin-07".into(),
                version: "0.11.0".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(answer.versions.len(), 1);
        assert_eq!(
            version_key(&answer.versions[0]),
            ("plugin-07".to_string(), "0.11.0".to_string())
        );
        assert_eq!(answer.launches.len(), 1);
        assert_eq!(answer.launches[0].instance_id, "plugin-07");
        assert!(answer.next_cursor.is_empty());

        let named = page(
            held,
            &PluginCatalogueRequest {
                name: "plugin-07".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(named.versions.len(), 12);
        assert!(named
            .versions
            .iter()
            .all(|v| version_key(v).0 == "plugin-07"));
    }

    #[test]
    fn a_page_size_bounds_the_records_and_the_cursor_carries_on() {
        let held = past_a_mebibyte();
        let pages = every_page(
            &held,
            PluginCatalogueRequest {
                page_size: 50,
                ..Default::default()
            },
        );
        // 264 versions and 22 launches, 50 a page at most.
        assert!(pages
            .iter()
            .all(|p| p.versions.len() + p.launches.len() <= 50));
        let total: usize = pages
            .iter()
            .map(|p| p.versions.len() + p.launches.len())
            .sum();
        assert_eq!(total, 264 + 22);
    }

    #[test]
    fn a_version_uploaded_between_pages_is_neither_repeated_nor_skipped_ahead() {
        let mut held = past_a_mebibyte();
        let first = page(held.clone(), &PluginCatalogueRequest::default()).unwrap();
        assert!(!first.next_cursor.is_empty());
        // One before the cursor, one after it.
        held.versions.push(version("aaa", "0.1.0", 2));
        held.versions.push(version("zzz", "0.1.0", 2));
        let mut request = PluginCatalogueRequest {
            cursor: first.next_cursor.clone(),
            ..Default::default()
        };
        let mut rest = Vec::new();
        loop {
            let answer = page(held.clone(), &request).unwrap();
            rest.extend(answer.versions.iter().map(version_key));
            if answer.next_cursor.is_empty() {
                break;
            }
            request.cursor = answer.next_cursor;
        }
        let seen: Vec<_> = first.versions.iter().map(version_key).collect();
        assert!(rest.iter().all(|key| !seen.contains(key)), "repeated");
        assert!(rest.contains(&("zzz".to_string(), "0.1.0".to_string())));
    }

    #[test]
    fn a_record_past_the_budget_is_a_page_of_its_own() {
        let mut held = PluginCatalogue::default();
        held.versions.push(version("big", "1.0.0", 400));
        held.versions.push(version("small", "1.0.0", 1));
        let budget = 8 * 1024;
        let first = page_within(held.clone(), &PluginCatalogueRequest::default(), budget).unwrap();
        assert_eq!(first.versions.len(), 1);
        let second = page_within(
            held,
            &PluginCatalogueRequest {
                cursor: first.next_cursor,
                ..Default::default()
            },
            budget,
        )
        .unwrap();
        assert_eq!(version_key(&second.versions[0]).0, "small");
        assert!(second.next_cursor.is_empty());
    }

    #[test]
    fn what_the_request_gets_wrong_is_refused_naming_the_field() {
        let held = PluginCatalogue::default();
        for (request, field) in [
            (
                PluginCatalogueRequest {
                    version: "0.1.0".into(),
                    ..Default::default()
                },
                "version",
            ),
            (
                PluginCatalogueRequest {
                    page_size: -1,
                    ..Default::default()
                },
                "page_size",
            ),
            (
                PluginCatalogueRequest {
                    cursor: "somewhere".into(),
                    ..Default::default()
                },
                "cursor",
            ),
        ] {
            let refused = page(held.clone(), &request).unwrap_err();
            let (status, fields, _) = meridian_bus::read_refusal_status(&refused).unwrap();
            assert_eq!(status, RefusalStatus::InvalidArgument);
            assert_eq!(fields, vec![field.to_string()]);
        }
    }
}
