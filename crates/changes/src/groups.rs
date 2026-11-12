//! Writer groups (step 3d; spec/a-deployment-scales-out-and-fails-over-by-
//! configuration sections 6.1, 6.3 and 6.7): a store's partitions owned by
//! `lake.writerGroups` or `bor.writerGroups` groups, each with its own
//! writer, standby and lease.
//!
//! **Ownership by rendezvous hashing** (highest random weight): a partition
//! belongs to the group whose weight for it is highest, the weight the first
//! eight bytes of SHA-256 over `<partition>/<group>`. A count raised or
//! lowered moves only the partitions whose highest weight was the group
//! added or removed; `hash mod N` would move nearly all (Kafka's and NATS's
//! warning). `control` is always group 0's, whatever the count.
//!
//! With one group, the default, group 0 owns every partition and nothing is
//! handed on.

use sha2::{Digest, Sha256};

/// This member's writer group (`MERIDIAN_WRITER_GROUP`, approved
/// 2026-11-12): 0 where the chart says nothing.
pub const GROUP_VAR: &str = "MERIDIAN_WRITER_GROUP";

/// Its store's count of writer groups (`MERIDIAN_WRITER_GROUPS`, from
/// `lake.writerGroups` or `bor.writerGroups`): 1 where the chart says
/// nothing.
pub const COUNT_VAR: &str = "MERIDIAN_WRITER_GROUPS";

/// Why a writer took its group's partitions over at start, unled by a
/// lease: they were assigned to its group -- the store's count of groups
/// changed -- and held at an epoch not its group's alone.
pub const ASSIGNED: &str = "the writer group's partitions were assigned to it";

/// The most writer groups a store takes: more than a deployment's writers
/// would ever need, and a count CI does not test is never offered.
pub const MOST: u32 = 64;

/// A member's writer group and its store's count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Groups {
    mine: u32,
    count: u32,
}

impl Default for Groups {
    fn default() -> Self {
        Self::one()
    }
}

impl Groups {
    /// One group, which owns every partition: the default.
    pub const fn one() -> Self {
        Self { mine: 0, count: 1 }
    }

    /// Group `mine` of `count`; refused where it is not one of them.
    pub fn new(mine: u32, count: u32) -> Result<Self, String> {
        if count == 0 || count > MOST {
            return Err(format!(
                "a store's writer groups are {count}; from 1 to {MOST}"
            ));
        }
        if mine >= count {
            return Err(format!(
                "this member's writer group is {mine}, and its store has {count}: \
                 groups are numbered from 0 below the count"
            ));
        }
        Ok(Self { mine, count })
    }

    /// From what the chart sets: [`GROUP_VAR`] and [`COUNT_VAR`].
    pub fn from_env() -> Result<Self, String> {
        let read = |name: &str, default: u32| -> Result<u32, String> {
            match std::env::var(name) {
                Err(_) => Ok(default),
                Ok(said) if said.trim().is_empty() => Ok(default),
                Ok(said) => said
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| format!("{name} is {said:?}; a whole number")),
            }
        };
        Self::new(read(GROUP_VAR, 0)?, read(COUNT_VAR, 1)?)
    }

    /// This member's group.
    pub fn mine(&self) -> u32 {
        self.mine
    }

    /// The store's count of groups.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Whether the store has more than one group: only then is anything
    /// handed on.
    pub fn several(&self) -> bool {
        self.count > 1
    }

    /// The group that owns `partition`.
    pub fn owner(&self, partition: &str) -> u32 {
        owner(partition, self.count)
    }

    /// Whether this member's group owns `partition`.
    pub fn owns(&self, partition: &str) -> bool {
        self.owner(partition) == self.mine
    }

    /// Whether this member's group owns the store's `control` partition:
    /// group 0, which does the store's own work -- its control records, its
    /// sweeps -- and serves its reads.
    pub fn holds_control(&self) -> bool {
        self.mine == 0
    }
}

/// The group of `count` that owns `partition`: `control` group 0's, every
/// other the group of highest weight for it.
pub fn owner(partition: &str, count: u32) -> u32 {
    if count <= 1 || partition == crate::CONTROL {
        return 0;
    }
    (0..count)
        .max_by_key(|group| (weight(partition, *group), std::cmp::Reverse(*group)))
        .unwrap_or(0)
}

/// A partition's weight for a group: the first eight bytes of SHA-256 over
/// `<partition>/<group>`, a constant of the product.
fn weight(partition: &str, group: u32) -> u64 {
    let digest = Sha256::digest(format!("{partition}/{group}").as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_group_owns_every_partition_and_control_is_group_zeros() {
        for partition in ["P0", "P1", "coinbase-1:daily", "control"] {
            assert_eq!(owner(partition, 1), 0);
        }
        for count in 2..8 {
            assert_eq!(owner("control", count), 0, "control at {count} groups");
        }
        assert!(Groups::one().owns("P0"));
        assert!(!Groups::one().several());
    }

    #[test]
    fn ownership_is_stable_and_a_group_added_takes_only_partitions_it_wins() {
        // A constant of the product: the book's P0 at two groups is group 1's.
        assert_eq!(owner("P0", 2), 1);
        let partitions: Vec<String> = (0..400).map(|n| format!("dataset-{n}")).collect();
        let at = |count: u32| -> Vec<u32> { partitions.iter().map(|p| owner(p, count)).collect() };
        let (two, three) = (at(2), at(3));
        for (before, after) in two.iter().zip(&three) {
            assert!(
                before == after || *after == 2,
                "a third group moves only what the third group takes"
            );
        }
        // Spread about evenly.
        let ones = two.iter().filter(|g| **g == 1).count();
        assert!((150..250).contains(&ones), "{ones} of 400 at group 1");
    }

    #[test]
    fn a_group_outside_its_count_is_refused() {
        assert!(Groups::new(2, 2).is_err());
        assert!(Groups::new(0, 0).is_err());
        assert!(Groups::new(0, MOST + 1).is_err());
        let g = Groups::new(1, 2).unwrap();
        assert!(g.owns("P0") && !g.owns("control") && !g.holds_control());
    }
}
