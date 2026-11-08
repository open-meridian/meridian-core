//! TLS on the deployment's internal paths (ruling 1084; step 3b part 4): the
//! one connector every Postgres client in the runtime uses, and the
//! certificates the broker's clients and the plugins' front doors are given.
//!
//! The chart turns it on with one value, `tls.internal.enabled`, which makes
//! a CA and the servers' certificates, and gives every component the CA's
//! certificate at [`CA_VAR`]. With it set, a client connects over TLS or not
//! at all: the database's connection asks for TLS and refuses a server that
//! offers none, and the server's certificate is verified against that CA and
//! the name connected to. Without it, nothing changes: a database URL that
//! names no `sslmode` connects as it always has, in the clear, and one that
//! names one is held to it, the server verified against the public roots.
//!
//! One connector, cached for the process, rather than one per call site: the
//! stores, the dashboard, the probe and the Jobs each open the same database
//! the same way.

use std::sync::{Arc, OnceLock};

use tokio_postgres_rustls::MakeRustlsConnect;

/// Where the chart puts the internal CA's certificate, when
/// `tls.internal.enabled` is on: a PEM file of one or more certificates.
pub const CA_VAR: &str = meridian_bus::INTERNAL_TLS_CA;

/// A server's own certificate chain and key, where a component serves TLS
/// itself (a sidecar's front door): PEM files.
pub const CERT_VAR: &str = "MERIDIAN_INTERNAL_TLS_CERT";
pub const KEY_VAR: &str = "MERIDIAN_INTERNAL_TLS_KEY";

/// The Postgres connector every client in the runtime uses, in place of
/// `NoTls`. It negotiates TLS only where the configuration asks for it
/// ([`database_tls_mode`]); asked, it verifies the server.
pub type DatabaseTls = MakeRustlsConnect;

/// The internal CA's file, when internal TLS is on.
pub fn internal_ca() -> Option<String> {
    std::env::var(CA_VAR)
        .ok()
        .filter(|path| !path.trim().is_empty())
}

/// Whether internal TLS is on for this process.
pub fn internal() -> bool {
    internal_ca().is_some()
}

/// The internal CA's certificates, read from [`CA_VAR`]'s file.
pub fn internal_roots() -> Result<Option<rustls::RootCertStore>, String> {
    let Some(path) = internal_ca() else {
        return Ok(None);
    };
    let mut roots = rustls::RootCertStore::empty();
    for certificate in certificates(&path)? {
        roots
            .add(certificate)
            .map_err(|failed| format!("the internal CA at {path} is not one: {failed}"))?;
    }
    if roots.is_empty() {
        return Err(format!("the internal CA at {path} holds no certificate"));
    }
    Ok(Some(roots))
}

/// Every certificate in a PEM file.
pub fn certificates(path: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    use rustls::pki_types::pem::PemObject as _;
    rustls::pki_types::CertificateDer::pem_file_iter(path)
        .and_then(|each| each.collect::<Result<Vec<_>, _>>())
        .map_err(|failed| format!("the certificates at {path} are unreadable: {failed}"))
}

/// The private key in a PEM file.
pub fn private_key(path: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>, String> {
    use rustls::pki_types::pem::PemObject as _;
    rustls::pki_types::PrivateKeyDer::from_pem_file(path)
        .map_err(|failed| format!("the key at {path} is unreadable: {failed}"))
}

/// The one TLS implementation the runtime builds with (see the workspace's
/// async-nats entry: one provider in the tree, named, never chosen by
/// default).
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A client's TLS configuration: the internal CA where internal TLS is on,
/// the public roots otherwise.
pub fn client_config() -> Result<rustls::ClientConfig, String> {
    let roots = match internal_roots()? {
        Some(roots) => roots,
        None => rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
    };
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|failed| failed.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// The Postgres connector, made once for the process.
pub fn database_tls() -> Result<DatabaseTls, std::io::Error> {
    static MADE: OnceLock<Result<DatabaseTls, String>> = OnceLock::new();
    MADE.get_or_init(|| client_config().map(MakeRustlsConnect::new))
        .clone()
        .map_err(std::io::Error::other)
}

/// The `sslmode` a connection is held to. With internal TLS on, `require`:
/// TLS or no connection, the server verified. Off, a URL that names a mode
/// keeps it, and one that does not connects in the clear as before (the
/// driver's own default, `prefer`, would try TLS against any server that
/// offers it, which this connector, unlike `NoTls`, would then hold to a
/// certificate).
pub fn database_tls_mode(config: &mut postgres::Config, named: bool) {
    if internal() {
        config.ssl_mode(postgres::config::SslMode::Require);
    } else if !named {
        config.ssl_mode(postgres::config::SslMode::Disable);
    }
}

/// Whether a database URL names its own `sslmode`.
pub fn names_ssl_mode(url: &str) -> bool {
    url.contains("sslmode")
}

/// A database configuration and the connector to open it with, held to
/// [`database_tls_mode`]: what every Postgres client in the runtime connects
/// through, a store's pool or a one-off connection (a probe, a Job).
pub fn secured(
    mut config: postgres::Config,
    named: bool,
) -> Result<(postgres::Config, DatabaseTls), std::io::Error> {
    database_tls_mode(&mut config, named);
    Ok((config, database_tls()?))
}

/// A server's TLS configuration from [`CERT_VAR`] and [`KEY_VAR`], when both
/// are set.
pub fn server_config() -> Result<Option<rustls::ServerConfig>, String> {
    let (Ok(cert), Ok(key)) = (std::env::var(CERT_VAR), std::env::var(KEY_VAR)) else {
        return Ok(None);
    };
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|failed| failed.to_string())?
        .with_no_client_auth()
        .with_single_cert(certificates(&cert)?, private_key(&key)?)
        .map_err(|failed| {
            format!("the certificate at {cert} and its key do not serve: {failed}")
        })?;
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_naming_its_mode_is_seen_to() {
        assert!(names_ssl_mode("postgres://a@b/c?sslmode=require"));
        assert!(!names_ssl_mode("postgres://a@b/c"));
    }

    #[test]
    fn off_and_unnamed_connects_in_the_clear_as_before() {
        // The variable is the chart's; unset in a unit test's process.
        if internal() {
            return;
        }
        let mut config: postgres::Config = "postgres://a@b/c".parse().unwrap();
        database_tls_mode(&mut config, false);
        assert_eq!(config.get_ssl_mode(), postgres::config::SslMode::Disable);
        let mut named: postgres::Config = "postgres://a@b/c?sslmode=require".parse().unwrap();
        database_tls_mode(&mut named, true);
        assert_eq!(named.get_ssl_mode(), postgres::config::SslMode::Require);
    }

    #[test]
    fn the_connector_is_made_without_a_ca() {
        if internal() {
            return;
        }
        assert!(database_tls().is_ok());
    }
}
