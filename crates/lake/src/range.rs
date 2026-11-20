//! The lake's hash ranges (step 3d; spec/a-deployment-scales-out-and-fails-
//! over-by-configuration section 6.1): a lake partition is a range of one
//! dataset's 16-bit hash space.
//!
//! - **The space** is the first two bytes of SHA-256 over a row's first
//!   subject's UTF-8 bytes: a constant of the product, never a chart value.
//! - **A full range** keeps its dataset's ID as its partition ID, so every
//!   partition before 3d keeps its name.
//! - **A split range** is named `<dataset>/<lo>-<hi>`, lowercase hex, `hi`
//!   inclusive (`…/0000-7fff`).
//! - **A row key's range** is the one its first version was written to: its
//!   anchor, the hash of that version's first subject, is kept with every
//!   version, so a key restated under the record that replaced its subject
//!   (followed by alias, ruled 2026-11-12) stays where it began.

/// The hash space's highest value: `0000` to `ffff`.
pub const TOP: u16 = u16::MAX;

/// A row's place in the hash space: the first two bytes of SHA-256 over its
/// first subject.
pub fn hash(subject: &str) -> u16 {
    let digest = meridian_changes::integrity::digest(subject.as_bytes());
    u16::from_be_bytes([digest[0], digest[1]])
}

/// A partition's ID: its dataset's for the full range, else
/// `<dataset>/<lo>-<hi>`.
pub fn id(dataset: &str, lo: u16, hi: u16) -> String {
    if lo == 0 && hi == TOP {
        dataset.to_string()
    } else {
        format!("{dataset}/{lo:04x}-{hi:04x}")
    }
}

/// A partition ID read: its dataset and its range. An ID that does not end
/// in a range is a dataset's full range.
pub fn parse(partition: &str) -> (&str, u16, u16) {
    if let Some((dataset, range)) = partition.rsplit_once('/') {
        if let Some((lo, hi)) = range.split_once('-') {
            let hex = |said: &str| {
                (said.len() == 4
                    && said
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                .then(|| u16::from_str_radix(said, 16).ok())
                .flatten()
            };
            if let (Some(lo), Some(hi)) = (hex(lo), hex(hi)) {
                if lo <= hi && !dataset.is_empty() {
                    return (dataset, lo, hi);
                }
            }
        }
    }
    (partition, 0, TOP)
}

/// The dataset a partition is a range of.
pub fn dataset_of(partition: &str) -> &str {
    parse(partition).0
}

/// Whether `partition` is one of `dataset`'s ranges, the full one among them.
pub fn is_of(partition: &str, dataset: &str) -> bool {
    dataset_of(partition) == dataset
}

/// A range's two halves at its midpoint, the lower first; none for a range
/// of one hash value, which cannot halve.
pub fn halves(lo: u16, hi: u16) -> Option<((u16, u16), (u16, u16))> {
    if lo >= hi {
        return None;
    }
    let mid = lo + (hi - lo) / 2;
    Some(((lo, mid), (mid + 1, hi)))
}

/// A row key's 32-bit hash (FNV-1a), stable across releases: what a page
/// holding a row by its place tells it from another range's row at the same
/// sequence by.
pub fn key_hash(row_key: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in row_key.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// One open range of a dataset, as its writer places rows in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub partition: String,
    pub lo: u16,
    pub hi: u16,
}

impl Open {
    pub fn holds(&self, anchor: u16) -> bool {
        self.lo <= anchor && anchor <= self.hi
    }
}

/// The open range of `ranges` holding `anchor`; the dataset's full range
/// where it has none open yet (a dataset not yet written).
pub fn placed<'a>(dataset: &'a str, ranges: &'a [Open], anchor: u16) -> std::borrow::Cow<'a, str> {
    match ranges.iter().find(|range| range.holds(anchor)) {
        Some(range) => std::borrow::Cow::Borrowed(range.partition.as_str()),
        None => std::borrow::Cow::Borrowed(dataset),
    }
}

/// The open ranges among a dataset's ranges `named` (`<dataset>/<lo>-<hi>`,
/// open and closed, each with its group): those no other of them lies
/// within, as a split's children lie within their parent. In hash order.
pub fn leaves(named: &[(String, u32)]) -> Vec<(Open, u32)> {
    let parsed: Vec<(Open, u32)> = named
        .iter()
        .map(|(partition, group)| {
            let (_, lo, hi) = parse(partition);
            (
                Open {
                    partition: partition.clone(),
                    lo,
                    hi,
                },
                *group,
            )
        })
        .collect();
    let mut open: Vec<(Open, u32)> = parsed
        .iter()
        .filter(|(range, _)| {
            !parsed.iter().any(|(other, _)| {
                other.partition != range.partition && range.lo <= other.lo && other.hi <= range.hi
            })
        })
        .cloned()
        .collect();
    open.sort_by_key(|(range, _)| range.lo);
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_ranges_are_those_no_other_lies_within() {
        let named = vec![
            ("d/0000-7fff".to_string(), 0),
            ("d/0000-3fff".to_string(), 1),
            ("d/4000-7fff".to_string(), 0),
            ("d/8000-ffff".to_string(), 1),
        ];
        let open: Vec<(String, u32)> = leaves(&named)
            .into_iter()
            .map(|(range, group)| (range.partition, group))
            .collect();
        assert_eq!(
            open,
            vec![
                ("d/0000-3fff".to_string(), 1),
                ("d/4000-7fff".to_string(), 0),
                ("d/8000-ffff".to_string(), 1)
            ]
        );
    }

    #[test]
    fn a_full_range_keeps_its_datasets_id_and_a_split_one_is_named_by_its_hex_range() {
        assert_eq!(id("truefx:live", 0, TOP), "truefx:live");
        assert_eq!(id("truefx:live", 0, 0x7fff), "truefx:live/0000-7fff");
        assert_eq!(id("truefx:live", 0x8000, TOP), "truefx:live/8000-ffff");
        assert_eq!(parse("truefx:live"), ("truefx:live", 0, TOP));
        assert_eq!(parse("truefx:live/8000-ffff"), ("truefx:live", 0x8000, TOP));
        assert_eq!(parse("truefx:live/0000-3fff"), ("truefx:live", 0, 0x3fff));
        // Not a range: upper case, the wrong width, reversed, no dataset.
        assert_eq!(parse("a/0000-7FFF").0, "a/0000-7FFF");
        assert_eq!(parse("a/000-7fff").0, "a/000-7fff");
        assert_eq!(parse("a/8000-0000").0, "a/8000-0000");
        assert_eq!(parse("/0000-7fff").0, "/0000-7fff");
        assert_eq!(dataset_of("a:b/0000-7fff"), "a:b");
        assert!(is_of("a:b/0000-7fff", "a:b"));
        assert!(is_of("a:b", "a:b"));
        assert!(!is_of("a:bc", "a:b"));
    }

    #[test]
    fn a_range_halves_at_its_midpoint_and_one_value_cannot() {
        assert_eq!(halves(0, TOP), Some(((0, 0x7fff), (0x8000, TOP))));
        assert_eq!(halves(0x8000, TOP), Some(((0x8000, 0xbfff), (0xc000, TOP))));
        assert_eq!(halves(4, 5), Some(((4, 4), (5, 5))));
        assert_eq!(halves(7, 7), None);
    }

    #[test]
    fn the_hash_is_the_first_two_bytes_of_sha256_of_the_first_subject() {
        // SHA-256("abc") = ba7816bf...
        assert_eq!(hash("abc"), 0xba78);
        let ranges = vec![
            Open {
                partition: "d/0000-7fff".into(),
                lo: 0,
                hi: 0x7fff,
            },
            Open {
                partition: "d/8000-ffff".into(),
                lo: 0x8000,
                hi: TOP,
            },
        ];
        assert_eq!(placed("d", &ranges, 0xba78), "d/8000-ffff");
        assert_eq!(placed("d", &ranges, 0x0001), "d/0000-7fff");
        assert_eq!(placed("d", &[], 0x0001), "d");
    }
}
