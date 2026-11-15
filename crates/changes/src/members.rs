//! A store's members, as each says it is serving (step 3d; spec/a-deployment-
//! scales-out-and-fails-over-by-configuration sections 6.4 and 6.8; ruled
//! 2026-11-15, item 9): `lake_member` and `book_member`, in the store's
//! database 0. Every member -- a writer, a standby, a reader -- keeps its own
//! row there, seen again every few seconds, with the revision it runs.
//!
//! **Why.** A raised `<store>.databases` rolls out a member at a time. A
//! member started before it reads one database. Once the control group's
//! sweep moves a partition home to another, that member reads the database
//! the partition left, and answers from it as though nothing were there (PR
//! CI 43656183991, ha: an account read as empty, twice). So the sweep moves
//! a partition home only once the rollout is complete: every member seen
//! within the last lease interval runs the sweep's own revision.
//!
//! **A revision** is the release's version and every setting that changes
//! where a member reads or writes, as a string a person can read:
//! `0.1.540/db2/wg2/p4` -- the store's databases, its writer groups and,
//! for the book, its partitions. An upgrade of the version alone, of a count
//! alone or of the groups alone is each held until every member seen has
//! rolled.
//!
//! A row not seen within the interval is ignored: its member has stopped,
//! or cannot reach database 0, and then serves no read from it either. A
//! row not seen for a day is let go.

use postgres::GenericClient;

/// How long a member's row is kept once it is no longer seen: a day, long
/// enough to read what ran after an upgrade went wrong.
pub const KEPT_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// A member's revision: `version`, the store's count of `databases` and of
/// writer `groups`, and the book's count of `partitions` (none for the
/// lake).
pub fn revision(version: &str, databases: u32, groups: u32, partitions: Option<u32>) -> String {
    match partitions {
        Some(partitions) => format!("{version}/db{databases}/wg{groups}/p{partitions}"),
        None => format!("{version}/db{databases}/wg{groups}"),
    }
}

/// A store's table of its members: `lake_member` or `book_member`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table(&'static str);

/// The lake's.
pub const LAKE: Table = Table("lake_member");
/// The book's.
pub const BOOK: Table = Table("book_member");

/// A member seen at another revision than the one asked of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Behind {
    pub member: String,
    pub revision: String,
    pub seen_at_ns: i64,
}

impl Table {
    /// The table's name.
    pub fn name(&self) -> &'static str {
        self.0
    }

    /// `member` seen at `now_ns`, running `revision` since `started_at_ns`:
    /// its own row made or brought up to date, and the rows not seen for a
    /// day let go.
    pub fn seen(
        &self,
        client: &mut impl GenericClient,
        member: &str,
        revision: &str,
        started_at_ns: i64,
        now_ns: i64,
    ) -> Result<(), postgres::Error> {
        client.execute(
            &format!(
                "INSERT INTO {} (member, revision, started_at_ns, seen_at_ns)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (member) DO UPDATE
                    SET revision = EXCLUDED.revision, started_at_ns = EXCLUDED.started_at_ns,
                        seen_at_ns = EXCLUDED.seen_at_ns",
                self.0
            ),
            &[&member, &revision, &started_at_ns, &now_ns],
        )?;
        client.execute(
            &format!("DELETE FROM {} WHERE seen_at_ns < $1", self.0),
            &[&(now_ns - KEPT_NS)],
        )?;
        Ok(())
    }

    /// The members seen within `within_ns` of `now_ns` that run another
    /// revision than `revision`: none, and the store's rollout is complete.
    pub fn behind(
        &self,
        client: &mut impl GenericClient,
        revision: &str,
        now_ns: i64,
        within_ns: i64,
    ) -> Result<Vec<Behind>, postgres::Error> {
        Ok(client
            .query(
                &format!(
                    "SELECT member, revision, seen_at_ns FROM {}
                      WHERE seen_at_ns >= $1 AND revision <> $2 ORDER BY member",
                    self.0
                ),
                &[&(now_ns - within_ns), &revision],
            )?
            .iter()
            .map(|row| Behind {
                member: row.get(0),
                revision: row.get(1),
                seen_at_ns: row.get(2),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_names_the_version_and_each_count() {
        assert_eq!(
            revision("0.1.540", 2, 2, Some(4)),
            "0.1.540/db2/wg2/p4",
            "the book's"
        );
        assert_eq!(
            revision("0.1.540", 1, 2, None),
            "0.1.540/db1/wg2",
            "the lake's"
        );
        assert_ne!(
            revision("0.1.540", 1, 1, None),
            revision("0.1.540", 2, 1, None),
            "the count alone raised"
        );
    }
}
