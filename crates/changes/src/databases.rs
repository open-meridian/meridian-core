//! A database per writer group (step 3d; spec/a-deployment-scales-out-and-
//! fails-over-by-configuration section 6.4, ruled 2026-11-10):
//! `lake.databases` and `bor.databases` databases, writer group `g` writing
//! to database `g mod count`.
//!
//! - **Database 0** is the store's home: its `control` partition and its
//!   assignments. Database 0 keeps today's URL variable
//!   (`MERIDIAN_<STORE>_DATABASE_URL`); database `d` past it is read from
//!   `MERIDIAN_<STORE>_DATABASE_URL_<d>`, and its record-read replica from
//!   `MERIDIAN_<STORE>_READ_DATABASE_URL_<d>` (approved 2026-11-14).
//! - **Writes** go to the writer group's own database alone; **reads**
//!   across databases fan out and merge in the reader.
//! - **The lowest group of each database leads it**: group `d` for database
//!   `d`. It follows that database's timeline and raises every epoch there,
//!   as group 0 does database 0's (section 3).
//!
//! With one database, the default, nothing here changes anything.

use crate::groups::Groups;

/// The most databases a store takes: never more than its writer groups.
pub const MOST: u32 = crate::groups::MOST;

/// A store's count of databases and its member's URL for each, database 0
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Databases {
    urls: Vec<String>,
}

/// The count's variable for `store` (`lake` or `bor`): `MERIDIAN_LAKE_DATABASES`,
/// `MERIDIAN_BOR_DATABASES` (approved 2026-11-14, by the ruled pattern).
pub fn count_var(store: &str) -> String {
    format!("MERIDIAN_{}_DATABASES", store.to_ascii_uppercase())
}

/// Database `d`'s URL variable for `store`: today's for database 0,
/// `MERIDIAN_<STORE>_DATABASE_URL_<d>` past it.
pub fn url_var(store: &str, d: u32) -> String {
    let store = store.to_ascii_uppercase();
    if d == 0 {
        format!("MERIDIAN_{store}_DATABASE_URL")
    } else {
        format!("MERIDIAN_{store}_DATABASE_URL_{d}")
    }
}

/// Database `d`'s record-read replica's URL variable for `store`.
pub fn read_url_var(store: &str, d: u32) -> String {
    let store = store.to_ascii_uppercase();
    if d == 0 {
        format!("MERIDIAN_{store}_READ_DATABASE_URL")
    } else {
        format!("MERIDIAN_{store}_READ_DATABASE_URL_{d}")
    }
}

impl Databases {
    /// One database, at `url`: the default.
    pub fn one(url: impl Into<String>) -> Self {
        Self {
            urls: vec![url.into()],
        }
    }

    /// The databases at `urls`, database 0 first; refused where there are
    /// none or more than [`MOST`].
    pub fn new(urls: Vec<String>) -> Result<Self, String> {
        if urls.is_empty() || urls.len() > MOST as usize {
            return Err(format!(
                "a store's databases are {}; from 1 to {MOST}",
                urls.len()
            ));
        }
        Ok(Self { urls })
    }

    /// From what the chart sets for `store`: its count ([`count_var`], 1
    /// where unset) and each database's URL, database 0's `database_0`
    /// (already read) and each past it its own variable, refused where one
    /// is missing.
    pub fn from_env(store: &str, database_0: &str) -> Result<Self, String> {
        let name = count_var(store);
        let count = match std::env::var(&name) {
            Err(_) => 1,
            Ok(said) if said.trim().is_empty() => 1,
            Ok(said) => said
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{name} is {said:?}; a whole number of databases"))?,
        };
        if count == 0 || count > MOST {
            return Err(format!("{name} is {count}; from 1 to {MOST}"));
        }
        let mut urls = vec![database_0.to_string()];
        for d in 1..count {
            let var = url_var(store, d);
            match std::env::var(&var) {
                Ok(url) if !url.trim().is_empty() => urls.push(url),
                _ => {
                    return Err(format!(
                    "{name} is {count}, and {var} names no database: database {d}'s URL is missing"
                ))
                }
            }
        }
        Self::new(urls)
    }

    /// The store's count of databases.
    pub fn count(&self) -> u32 {
        self.urls.len() as u32
    }

    /// Whether the store has more than one.
    pub fn several(&self) -> bool {
        self.urls.len() > 1
    }

    /// Each database's URL, database 0 first.
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// The database writer group `group` writes to: `group mod count`.
    pub fn of_group(&self, group: u32) -> u32 {
        of_group(group, self.count())
    }

    /// The database holding `partition` with `groups`' count of writer
    /// groups: its owner's.
    pub fn of_partition(&self, groups: &Groups, partition: &str) -> u32 {
        self.of_group(groups.owner(partition))
    }
}

/// The database writer group `group` writes to, of `count`.
pub fn of_group(group: u32, count: u32) -> u32 {
    if count <= 1 {
        0
    } else {
        group % count
    }
}

/// Whether writer group `group` leads its database of `count`: the lowest
/// group writing to it, which follows its timeline and raises every epoch
/// there (group 0 for database 0).
pub fn leads(group: u32, count: u32) -> bool {
    group < count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_writes_to_its_number_modulo_the_count_and_the_lowest_leads() {
        assert_eq!(of_group(0, 1), 0);
        assert_eq!(of_group(3, 1), 0);
        assert_eq!(of_group(3, 2), 1);
        assert_eq!(of_group(2, 2), 0);
        assert!(leads(0, 1) && !leads(1, 1));
        assert!(leads(1, 2) && !leads(3, 2));
    }

    #[test]
    fn database_zero_keeps_its_variable_and_each_past_it_has_its_own() {
        assert_eq!(url_var("lake", 0), "MERIDIAN_LAKE_DATABASE_URL");
        assert_eq!(url_var("bor", 1), "MERIDIAN_BOR_DATABASE_URL_1");
        assert_eq!(read_url_var("lake", 0), "MERIDIAN_LAKE_READ_DATABASE_URL");
        assert_eq!(read_url_var("lake", 2), "MERIDIAN_LAKE_READ_DATABASE_URL_2");
        assert_eq!(count_var("bor"), "MERIDIAN_BOR_DATABASES");
        assert!(Databases::new(Vec::new()).is_err());
        assert_eq!(Databases::one("u").count(), 1);
    }
}
