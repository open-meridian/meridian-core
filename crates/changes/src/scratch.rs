//! What a store's test makes beside the test database's own -- a schema, or
//! a database -- dropped with the last clone of the URL that reaches it, a
//! failed test's too, as its stack unwinds (PO 65, 2026-11-17: runs left
//! `bor_fence_*` and `lake_*` schemas and `lake2_*` databases behind, and a
//! development database that outlives its runs kept every one). `make
//! test-store` fails a run that leaves any. Tests only: behind the
//! `scratch` feature, which the stores' tests turn on.

use std::sync::Arc;

/// A schema or a database a test made, by the URL that reaches it.
#[derive(Clone)]
pub struct Scratch(Arc<Made>);

/// What was made, its name, the database it was made from, and how it is
/// dropped.
struct Made {
    url: String,
    name: String,
    from: String,
    drop: String,
}

impl Scratch {
    /// Schema `name` made in the database at `url`, reached by a URL whose
    /// search path is it.
    pub fn schema(url: &str, name: &str) -> Scratch {
        made(url, &format!("CREATE SCHEMA {name}"));
        let joined = if url.contains('?') { "&" } else { "?" };
        Scratch(Arc::new(Made {
            url: format!("{url}{joined}options=-csearch_path%3D{name}"),
            name: name.to_string(),
            from: url.to_string(),
            drop: format!("DROP SCHEMA IF EXISTS {name} CASCADE"),
        }))
    }

    /// Database `name` made on the server of the database at `url`, and
    /// reached there.
    pub fn database(url: &str, name: &str) -> Scratch {
        let (server, _) = url
            .rsplit_once('/')
            .expect("a database URL names its database");
        made(url, &format!("CREATE DATABASE {name}"));
        Scratch(Arc::new(Made {
            url: format!("{server}/{name}"),
            name: name.to_string(),
            from: url.to_string(),
            // FORCE ends the connections a store's pool still holds there.
            drop: format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        }))
    }

    /// The schema's or the database's name.
    pub fn name(&self) -> &str {
        &self.0.name
    }
}

fn made(url: &str, statement: &str) {
    postgres::Client::connect(url, postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(statement)
        .unwrap_or_else(|failed| panic!("{statement}: {failed}"));
}

impl std::ops::Deref for Scratch {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0.url
    }
}

impl std::fmt::Display for Scratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.url)
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        let (from, statement) = (self.from.clone(), self.drop.clone());
        // On a thread of its own, so a sync client is never dropped inside
        // a runtime. A store still in a transaction there holds a lock the
        // drop waits on: it waits so long, then tries again, and says so.
        let _ = std::thread::spawn(move || {
            for _ in 0..5 {
                // Each on its own: a database is never dropped inside the
                // transaction a query of several statements is.
                let dropped =
                    postgres::Client::connect(&from, postgres::NoTls).and_then(|mut admin| {
                        admin.batch_execute("SET lock_timeout = '10s'")?;
                        admin.batch_execute(&statement)
                    });
                match dropped {
                    Ok(()) => return,
                    Err(failed) => eprintln!("{statement}: {failed}; trying again"),
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .join();
    }
}

/// What an async test holds over a store -- the store, a writer, a lake, a
/// mover, the databases under them -- dropped off the runtime wherever its
/// test ends, a failed one's too. A store's pool closes sync clients, and a
/// sync client closed on a runtime's thread panics: in a test already
/// unwinding that is a panic in a destructor, which ends the process before
/// any [`Scratch`] drops what it made (merge-queue run 44594070565: one
/// failed attempt of a lake test left its two `lake2_*` databases, and the
/// run failed on them after the test's retry passed). Reads as what it
/// holds.
pub struct OffRuntime<T: Send>(Option<T>);

impl<T: Send> OffRuntime<T> {
    /// `held`, dropped off the runtime when this goes.
    pub fn new(held: T) -> OffRuntime<T> {
        OffRuntime(Some(held))
    }
}

impl<T: Send> std::ops::Deref for OffRuntime<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.as_ref().expect("held until dropped")
    }
}

impl<T: Send> std::ops::DerefMut for OffRuntime<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("held until dropped")
    }
}

impl<T: Send> Drop for OffRuntime<T> {
    fn drop(&mut self) {
        let held = self.0.take();
        // On a thread of its own, as a `Scratch` drops what it made: never
        // on the runtime's, whatever kind of runtime its test runs on.
        std::thread::scope(|scope| {
            scope.spawn(move || drop(held));
        });
    }
}
