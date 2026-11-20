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

/// A member's row, as it last said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    pub member: String,
    pub revision: String,
    pub started_at_ns: i64,
    pub seen_at_ns: i64,
    /// None for a member of the release before stage T's.
    pub writer_group: Option<u32>,
    /// `writer`, `standby` or `reader`; empty until it serves.
    pub mode: String,
}

/// What a writer told to stop watches its store's members for (step 3d,
/// stage T; section 15: a rollout keeps every public topic served): a
/// writer serving the store's public topics that will not stop with it.
///
/// No clock is compared with another pod's (stage T's review, findings 9
/// and 10). Each row is stamped by its own member's clock, so "seen within
/// six seconds" and "started after I was told" each set one pod's time
/// against another's: a clock behind was never fresh, and one ahead made a
/// member about to stop read as new. Instead:
/// - **serving now** is a row said again between two of this watch's own
///   looks: its `seen_at_ns` moved, by its own member's clock alone;
/// - **not stopping with me** is a process this watch did not find at its
///   first look -- another member, or the same member started again -- or
///   one running another revision than the watcher's.
///
/// A member that died is never said again, so it is never serving, however
/// long its row is kept.
#[derive(Debug)]
pub struct Watch {
    writer_group: u32,
    revision: String,
    /// Whether its own group's writer counts: where no lease fences the
    /// group, its replacement serves beside it and takes its writes.
    own_group_too: bool,
    /// Each member's start as first looked at; none until the first look.
    first: Option<std::collections::HashMap<String, i64>>,
    /// Each member's `seen_at_ns` at the look before.
    before: std::collections::HashMap<String, i64>,
}

impl Watch {
    /// A watch by a writer of `writer_group` running `revision`.
    pub fn new(writer_group: u32, revision: &str, own_group_too: bool) -> Self {
        Self {
            writer_group,
            revision: revision.to_string(),
            own_group_too,
            first: None,
            before: std::collections::HashMap::new(),
        }
    }

    /// One look at the store's other members ([`Table::others`]): whether
    /// a writer serves that will not stop with the watcher. Never at the
    /// first look, which only notes who is there.
    pub fn serves(&mut self, others: &[Said]) -> bool {
        let before = std::mem::take(&mut self.before);
        self.before = others
            .iter()
            .map(|said| (said.member.clone(), said.seen_at_ns))
            .collect();
        let Some(first) = self.first.as_ref() else {
            self.first = Some(
                others
                    .iter()
                    .map(|said| (said.member.clone(), said.started_at_ns))
                    .collect(),
            );
            return false;
        };
        others.iter().any(|said| {
            let a_writer = said.mode == "writer"
                && said
                    .writer_group
                    .is_some_and(|group| self.own_group_too || group != self.writer_group);
            // Said again since the look before; a row not there then was
            // written since.
            let said_again = before
                .get(&said.member)
                .is_none_or(|seen| said.seen_at_ns != *seen);
            let anew = said.revision != self.revision
                || first.get(&said.member) != Some(&said.started_at_ns);
            a_writer && said_again && anew
        })
    }
}

/// What a standby watches for before it takes its group's lease (step 3d;
/// ruled 2026-11-16 for c30d-t4): a writer or standby of its own writer
/// group that runs another revision and started after this standby first looked -- a
/// rollout's new member, there to take the lease at the new count. An old
/// member that took it would write at the count it runs, and its takeover
/// would fence the writers of the groups the new count added (CI
/// 43788516279, "ha groups raised": group 1's lake writer took its lease
/// by lapse three times).
///
/// No clock is compared with another pod's, as in [`Watch`]: newer is a
/// process this watch did not find at its first look, and there now is a
/// row said again between two of this watch's own looks.
#[derive(Debug)]
pub struct Newer {
    writer_group: u32,
    revision: String,
    first: Option<std::collections::HashMap<String, i64>>,
    before: std::collections::HashMap<String, i64>,
}

impl Newer {
    /// A watch by a standby of `writer_group` running `revision`.
    pub fn new(writer_group: u32, revision: &str) -> Self {
        Self {
            writer_group,
            revision: revision.to_string(),
            first: None,
            before: std::collections::HashMap::new(),
        }
    }

    /// One look at the store's other members ([`Table::others`]): whether
    /// a newer member of its group is there now. Never at the first look,
    /// which only notes who is there.
    pub fn there(&mut self, others: &[Said]) -> bool {
        let before = std::mem::take(&mut self.before);
        self.before = others
            .iter()
            .map(|said| (said.member.clone(), said.seen_at_ns))
            .collect();
        let Some(first) = self.first.as_ref() else {
            self.first = Some(
                others
                    .iter()
                    .map(|said| (said.member.clone(), said.started_at_ns))
                    .collect(),
            );
            return false;
        };
        others.iter().any(|said| {
            // A writer or a standby of its group: a reader never takes the
            // lease (the review's M3).
            let mine = said.writer_group == Some(self.writer_group)
                && (said.mode == "writer" || said.mode == "standby");
            let said_again = before
                .get(&said.member)
                .is_none_or(|seen| said.seen_at_ns != *seen);
            let started_since = first.get(&said.member) != Some(&said.started_at_ns);
            mine && said.revision != self.revision && said_again && started_since
        })
    }
}

impl Table {
    /// The table's name.
    pub fn name(&self) -> &'static str {
        self.0
    }

    /// `member` seen at `now_ns`, running `revision` since `started_at_ns`:
    /// its own row made or brought up to date, and the rows not seen for a
    /// day let go. What it serves as is left as it was while the process is
    /// the same, and cleared where `started_at_ns` changed: another process
    /// under the member's name -- its container restarted -- serves nothing
    /// until it says so (stage T's review, finding 9).
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
                "INSERT INTO {0} AS held (member, revision, started_at_ns, seen_at_ns)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (member) DO UPDATE
                    SET revision = EXCLUDED.revision, started_at_ns = EXCLUDED.started_at_ns,
                        seen_at_ns = EXCLUDED.seen_at_ns,
                        mode = CASE WHEN held.started_at_ns = EXCLUDED.started_at_ns
                                    THEN held.mode ELSE '' END",
                self.0
            ),
            &[&member, &revision, &started_at_ns, &now_ns],
        )?;
        self.let_go(client, now_ns)
    }

    /// The same, with what `member` serves as (step 3d, stage T;
    /// `writer_group` and `mode` approved 2026-11-15): its writer group and
    /// the mode it serves in -- `writer`, `standby` or `reader` -- empty
    /// until it serves. One statement: a row never holds one process's
    /// start beside another's mode (stage T's review, finding 9).
    #[allow(clippy::too_many_arguments)]
    pub fn says(
        &self,
        client: &mut impl GenericClient,
        member: &str,
        revision: &str,
        started_at_ns: i64,
        now_ns: i64,
        writer_group: u32,
        mode: &str,
    ) -> Result<(), postgres::Error> {
        client.execute(
            &format!(
                "INSERT INTO {} (member, revision, started_at_ns, seen_at_ns, writer_group, mode)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (member) DO UPDATE
                    SET revision = EXCLUDED.revision, started_at_ns = EXCLUDED.started_at_ns,
                        seen_at_ns = EXCLUDED.seen_at_ns,
                        writer_group = EXCLUDED.writer_group, mode = EXCLUDED.mode",
                self.0
            ),
            &[
                &member,
                &revision,
                &started_at_ns,
                &now_ns,
                &(writer_group as i32),
                &mode,
            ],
        )?;
        self.let_go(client, now_ns)
    }

    fn let_go(&self, client: &mut impl GenericClient, now_ns: i64) -> Result<(), postgres::Error> {
        client.execute(
            &format!("DELETE FROM {} WHERE seen_at_ns < $1", self.0),
            &[&(now_ns - KEPT_NS)],
        )?;
        Ok(())
    }

    /// Every member's row but `member`'s own, as each last said it: what a
    /// writer told to stop reads, look after look ([`Watch`]).
    pub fn others(
        &self,
        client: &mut impl GenericClient,
        member: &str,
    ) -> Result<Vec<Said>, postgres::Error> {
        Ok(client
            .query(
                &format!(
                    "SELECT member, revision, started_at_ns, seen_at_ns, writer_group, mode
                       FROM {} WHERE member <> $1 ORDER BY member",
                    self.0
                ),
                &[&member],
            )?
            .iter()
            .map(|row| Said {
                member: row.get(0),
                revision: row.get(1),
                started_at_ns: row.get(2),
                seen_at_ns: row.get(3),
                writer_group: row.get::<_, Option<i32>>(4).map(|group| group as u32),
                mode: row.get(5),
            })
            .collect())
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

    fn said(member: &str, revision: &str, started: i64, seen: i64, group: u32, mode: &str) -> Said {
        Said {
            member: member.into(),
            revision: revision.into(),
            started_at_ns: started,
            seen_at_ns: seen,
            writer_group: Some(group),
            mode: mode.into(),
        }
    }

    /// A newer member is one of the standby's own group, at another
    /// revision, started since its first look and said again (ruled
    /// 2026-11-16).
    #[test]
    fn a_newer_member_is_one_of_its_group_at_another_revision_started_since_and_said_again() {
        let mut newer = Newer::new(0, "v/db1/wg1");
        let old = said("lake-old-b", "v/db1/wg1", 1, 10, 0, "standby");
        assert!(
            !newer.there(std::slice::from_ref(&old)),
            "never at the first look"
        );
        // A member of its group at the new count, started since.
        let new = said("lake-new-a", "v/db1/wg2", 50, 60, 0, "standby");
        assert!(newer.there(&[old.clone(), new.clone()]));
        // Not said again since: not there now.
        assert!(!newer.there(&[old.clone(), new.clone()]));
        let again = said("lake-new-a", "v/db1/wg2", 50, 70, 0, "standby");
        assert!(newer.there(&[old.clone(), again]));
        // Another group's, or its own revision's, is never newer.
        let other_group = said("lake-g1-a", "v/db1/wg2", 50, 80, 1, "writer");
        let same = said("lake-old-c", "v/db1/wg1", 50, 80, 0, "standby");
        assert!(!newer.there(&[old.clone(), other_group, same]));
        // A reader of its group never takes a lease: never newer (the
        // review's M3).
        let reader = said("lake-reader-x", "v/db1/wg2", 50, 90, 0, "reader");
        assert!(
            !newer.there(&[old.clone(), reader]),
            "a reader counted as a newer member that would take the lease"
        );
        // One found at the first look is never newer, said again or not.
        let mut late = Newer::new(0, "v/db1/wg1");
        late.there(&[said("lake-new-a", "v/db1/wg2", 50, 60, 0, "standby")]);
        assert!(!late.there(&[said("lake-new-a", "v/db1/wg2", 50, 70, 0, "standby")]));
    }

    /// A writer of group 1 told to stop, every group rolled at once: it
    /// stops for the first group's new writer once that serves and is said
    /// again, never for the old one about to stop with it, a standby, a
    /// reader, its own group's, or one holding its lease not yet serving.
    #[test]
    fn a_watch_finds_another_groups_writer_serving_anew() {
        let mut watch = Watch::new(1, "r1", false);
        let old = |seen| said("zero-old", "r1", 10, seen, 0, "writer");
        assert!(
            !watch.serves(&[old(100)]),
            "the first look only notes who is there"
        );
        assert!(
            !watch.serves(&[old(102)]),
            "the old writer, said again: it stops too"
        );
        let mut rows = vec![
            old(104),
            said("one-new", "r1", 500, 104, 1, "writer"),
            said("zero-standby", "r1", 500, 104, 0, "standby"),
            said("zero-reader", "r1", 500, 104, 0, "reader"),
            said("zero-new", "r1", 500, 104, 0, ""),
        ];
        assert!(
            !watch.serves(&rows),
            "none is another group's writer serving anew"
        );
        rows[4] = said("zero-new", "r1", 500, 106, 0, "writer");
        assert!(watch.serves(&rows), "the first group's new writer serves");
    }

    /// Finding 10: nothing is read off another pod's clock. A new writer
    /// whose clock is an hour behind the watcher's, or a day ahead, is found
    /// alike; and one that was there at the first look is not new however
    /// late its clock says it started.
    #[test]
    fn a_watch_compares_no_clock_with_another_pods() {
        for skew in [-3_600_000_000_000_i64, 86_400_000_000_000] {
            let mut watch = Watch::new(1, "r1", false);
            // There at the first look, its start stamped "after" any tell.
            let stopping = |seen| said("zero-old", "r1", i64::MAX / 2, seen, 0, "writer");
            assert!(!watch.serves(&[stopping(1)]));
            assert!(
                !watch.serves(&[stopping(2)]),
                "there when told: it stops too"
            );
            let new = said("zero-new", "r1", 7 + skew, 9 + skew, 0, "writer");
            assert!(watch.serves(&[stopping(3), new]), "skew {skew}");
        }
    }

    /// Finding 9: a writer that died is never said again, so it never
    /// serves, whatever its row says; and a row at another revision counts
    /// only once it is said again.
    #[test]
    fn a_watch_takes_no_row_that_is_not_said_again() {
        let mut watch = Watch::new(1, "r1", false);
        let dead = said("zero-rolled", "r2", 5, 50, 0, "writer");
        assert!(!watch.serves(std::slice::from_ref(&dead)));
        for _ in 0..3 {
            assert!(
                !watch.serves(std::slice::from_ref(&dead)),
                "never said again"
            );
        }
        let alive = said("zero-rolled", "r2", 5, 52, 0, "writer");
        assert!(watch.serves(&[alive]), "another revision, said again");
    }

    /// Where no lease fences the group (stage T's review, finding 3), its
    /// own group's replacement serving is leave to stop too; with a lease it
    /// never is.
    #[test]
    fn a_watch_counts_its_own_groups_new_writer_only_where_asked() {
        for (own_too, found) in [(false, false), (true, true)] {
            let mut watch = Watch::new(1, "r1", own_too);
            assert!(!watch.serves(&[]));
            let new = said("one-new", "r1", 9, 9, 1, "writer");
            assert_eq!(watch.serves(&[new]), found, "own group too: {own_too}");
        }
    }

    /// The same member's name under a process started again is a new
    /// process: its container restarted.
    #[test]
    fn a_watch_takes_a_member_started_again_as_new() {
        let mut watch = Watch::new(1, "r1", false);
        assert!(!watch.serves(&[said("zero", "r1", 10, 100, 0, "writer")]));
        assert!(
            !watch.serves(&[said("zero", "r1", 20, 102, 0, "")]),
            "not serving yet"
        );
        assert!(watch.serves(&[said("zero", "r1", 20, 104, 0, "writer")]));
    }
}
