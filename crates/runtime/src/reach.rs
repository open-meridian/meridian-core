//! One attempt at a database, bounded from its start to its end.
//!
//! The Postgres client's own connect timeout covers each address's TCP
//! connect and nothing else: the lookup of the name before it and the
//! server's first words after it are outside it, so a resolver that drops its
//! packets, or a server that accepts and says nothing, held an attempt -- and
//! the wait that makes one at a time -- for as long as it liked (LazyFS run
//! 3's diagnosis, 2026-11-27).
//!
//! So an attempt is made where it can be stopped ([`Attempt::asked`]): on the
//! asynchronous client, under one bound, and dropped at that bound with its
//! socket. Nothing of an attempt given up on stays behind but the system's
//! own lookup of a name, where that is what was slow, which ends when the
//! resolver gives up. A caller that needs the blocking client afterwards
//! ([`Attempt::blocking`]) has no way to stop its connect, and is given it
//! from a thread of its own instead.
//!
//! A handshake that works and takes longer than the bound, every time, is
//! not answered: it counts as a database not answering yet, the attempt is
//! given up on, and the wait keeps trying (accepted, 2026-11-27).

use std::time::Duration;

use meridian_changes::tls::DatabaseTls;

use crate::preflight::with_causes;

/// What one attempt at a connection is given for each host its URL names,
/// start to finish: the lookup, the connect, the server's first words and,
/// where the attempt asks something, the answers. The client's own connect
/// timeout, each address's, is the same, so an address that drops what it is
/// sent leaves the next host its own share ([`Attempt::bound`]).
pub(crate) const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// Why no attempt can be made at a URL, whatever its database does.
#[derive(Debug)]
pub(crate) enum Unusable {
    Url(postgres::Error),
    Tls(std::io::Error),
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(failed) => write!(f, "the database URL is unreadable: {failed}"),
            Self::Tls(failed) => write!(f, "the database's TLS: {failed}"),
        }
    }
}

/// One attempt at the database a URL names.
pub(crate) struct Attempt {
    config: tokio_postgres::Config,
    tls: DatabaseTls,
}

impl Attempt {
    pub(crate) fn at(url: &str) -> Result<Self, Unusable> {
        let config = url.parse().map_err(Unusable::Url)?;
        Self::held(config, meridian_changes::tls::names_ssl_mode(url))
    }

    /// The attempt `config` describes, held to the deployment's TLS as every
    /// Postgres client in the runtime is ([`meridian_changes::tls::secured`]);
    /// `named` is whether its URL names its own `sslmode`.
    pub(crate) fn held(mut config: tokio_postgres::Config, named: bool) -> Result<Self, Unusable> {
        config.connect_timeout(CONNECT_WITHIN);
        // The socket's own bounds, as a store's pool sets them
        // (`meridian_changes::database_config`) and unless the URL says
        // otherwise: a connection cut after it was made is noticed in
        // seconds, not the kernel's quarter of an hour.
        if config.get_tcp_user_timeout().is_none() {
            config.tcp_user_timeout(CONNECT_WITHIN);
        }
        if config.get_keepalives_idle() == Duration::from_secs(2 * 60 * 60) {
            config
                .keepalives(true)
                .keepalives_idle(CONNECT_WITHIN)
                .keepalives_interval(Duration::from_secs(2))
                .keepalives_retries(2);
        }
        let (secured, tls) =
            meridian_changes::tls::secured(postgres::Config::from(config.clone()), named)
                .map_err(Unusable::Tls)?;
        config.ssl_mode(secured.get_ssl_mode());
        Ok(Self { config, tls })
    }

    /// The hosts the URL names.
    pub(crate) fn hosts(&self) -> Vec<String> {
        self.config
            .get_hosts()
            .iter()
            .filter_map(|host| match host {
                tokio_postgres::config::Host::Tcp(name) => Some(name.clone()),
                #[allow(unreachable_patterns)]
                _ => None,
            })
            .collect()
    }

    /// The whole attempt's bound: [`CONNECT_WITHIN`] for each host the URL
    /// names. The client tries them in order, so a first host that drops
    /// what it is sent is given up on at its connect timeout with the next
    /// one's share left. One name of several addresses is one host here: how
    /// many it has is not known before its lookup.
    pub(crate) fn bound(&self) -> Duration {
        CONNECT_WITHIN * u32::try_from(self.config.get_hosts().len().max(1)).unwrap_or(1)
    }

    /// What `ask` makes of one connection, the whole of it -- the connection
    /// and its answers -- within [`Attempt::bound`]; or why not, a failure
    /// said with its causes and a bound passed said as the step that was
    /// out. At the bound the attempt is dropped where it stands, and its
    /// socket closed with it.
    ///
    /// Called off any runtime, as the blocking client is: it runs one of its
    /// own for the attempt's length.
    pub(crate) fn asked<T>(
        &self,
        ask: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, String>,
    ) -> Result<T, String> {
        let limit = self.bound();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|failed| format!("an attempt at a connection could not start: {failed}"))?;
        // The lookup of a name is the system's, on a thread the runtime
        // starts for it, and the runtime panics where the system refuses it
        // one: answered as not yet, like any other attempt that failed.
        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(self.made(limit, ask))
        }));
        // Not waited for: a lookup still out ends when the resolver gives up.
        runtime.shutdown_background();
        answer.unwrap_or_else(|_| {
            Err(
                "the attempt at a connection ended without an answer: it could not go on, as \
                 said above"
                    .into(),
            )
        })
    }

    async fn made<T>(
        &self,
        limit: Duration,
        ask: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, String>,
    ) -> Result<T, String> {
        let until = tokio::time::Instant::now() + limit;
        let connecting = self.config.connect(self.tls.clone());
        let (client, connection) = match tokio::time::timeout_at(until, connecting).await {
            Ok(Ok(made)) => made,
            Ok(Err(failed)) => return Err(with_causes(&failed)),
            Err(_) => return Err(gave_up(limit)),
        };
        let driven = tokio::spawn(connection);
        let answer = tokio::time::timeout_at(until, ask(&client))
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "connected, and what was asked of the database had no answer within \
                     {limit:?} of the attempt's start"
                ))
            });
        // The session ended as a client ends one, where the server is still
        // listening for it.
        drop(client);
        let _ = tokio::time::timeout_at(until, driven).await;
        answer
    }

    /// The blocking client, connected within [`Attempt::bound`], for a
    /// caller whose work needs it; a failure said by `said`.
    ///
    /// The blocking client has no way to be stopped, so its connect is made
    /// on a thread of its own ([`within`]), and one given up on runs to its
    /// own end there. Nothing that waits without a limit of its own may use
    /// this: each attempt given up on holds its thread until what it waits
    /// on ends. Its callers make one attempt (the components' logins, the
    /// migration's grant) or stop at a deadline (the upgrade's preflight).
    pub(crate) fn blocking(
        &self,
        said: impl FnOnce(&postgres::Error) -> String,
    ) -> Result<postgres::Client, String> {
        let (config, tls) = (
            postgres::Config::from(self.config.clone()),
            self.tls.clone(),
        );
        within(self.bound(), move || config.connect(tls), said)
    }
}

fn gave_up(limit: Duration) -> String {
    format!("no connection within {limit:?}: the attempt was given up on")
}

/// `attempt`'s answer where it comes within `limit`, a failure said by
/// `said`; and that there was none where it does not. The attempt is made on
/// a thread of its own and, given up on, runs to its own end there, where
/// what it then made is dropped, off any runtime. A thread the system
/// refuses is an attempt that failed, never a panic.
pub(crate) fn within<T, E>(
    limit: Duration,
    attempt: impl FnOnce() -> Result<T, E> + Send + 'static,
    said: impl FnOnce(&E) -> String,
) -> Result<T, String>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let (tell, told) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let _ = tell.send(attempt());
        })
        .map_err(|failed| format!("no thread for an attempt at a connection: {failed}"))?;
    match told.recv_timeout(limit) {
        Ok(Ok(made)) => Ok(made),
        Ok(Err(failed)) => Err(said(&failed)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(gave_up(limit)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("the attempt at a connection ended without an answer".into())
        }
    }
}

/// Servers a test stands in for a database with, each on a port of this
/// machine's own.
#[cfg(test)]
pub(crate) mod fakes {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{channel, Receiver};

    /// A server that accepts and never says a word. Each connection is held
    /// open, and `ended` is told when its client closes one.
    pub(crate) fn silent() -> (u16, Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tell, ended) = channel();
        std::thread::spawn(move || {
            for accepted in listener.incoming() {
                let (mut stream, tell) = (accepted.unwrap(), tell.clone());
                std::thread::spawn(move || {
                    let mut heard = [0u8; 512];
                    // Read to the client's close, or its reset.
                    while matches!(stream.read(&mut heard), Ok(1..)) {}
                    let _ = tell.send(());
                });
            }
        });
        (port, ended)
    }

    /// A server that answers a session's start as Postgres does -- any login
    /// let in, ready for a query -- and never answers anything after it.
    pub(crate) fn starts_and_says_no_more() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for accepted in listener.incoming() {
                std::thread::spawn(move || started(accepted.unwrap()));
            }
        });
        port
    }

    fn started(mut stream: TcpStream) {
        // The startup message: its length, itself included, then the rest.
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).is_err() {
            return;
        }
        let mut rest = vec![0u8; (u32::from_be_bytes(length) as usize).saturating_sub(4)];
        if stream.read_exact(&mut rest).is_err() {
            return;
        }
        let mut said = Vec::new();
        said.extend_from_slice(b"R\0\0\0\x08\0\0\0\0"); // AuthenticationOk
        said.extend_from_slice(b"K\0\0\0\x0c\0\0\0\x01\0\0\0\x02"); // BackendKeyData
        said.extend_from_slice(b"Z\0\0\0\x05I"); // ReadyForQuery, idle
        if stream.write_all(&said).is_err() {
            return;
        }
        let mut heard = [0u8; 512];
        while matches!(stream.read(&mut heard), Ok(1..)) {}
    }

    /// A port whose connection attempts are dropped: a listener that never
    /// accepts, its queue of one filled. Kept, with what fills it, for as
    /// long as the port is to stay that way.
    pub(crate) fn drops_connections() -> (u16, (TcpListener, Vec<TcpStream>)) {
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Listening again sets the queue's length: none beyond the one the
        // kernel always keeps.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let at = listener.local_addr().unwrap();
        let mut filling = Vec::new();
        for _ in 0..16 {
            match TcpStream::connect_timeout(&at, std::time::Duration::from_millis(300)) {
                Ok(held) => filling.push(held),
                Err(_) => return (at.port(), (listener, filling)),
            }
        }
        panic!("a listener that never accepts took sixteen connections");
    }
}
