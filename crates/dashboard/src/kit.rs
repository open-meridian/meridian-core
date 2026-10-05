//! The plugin UI kit, served at `/.meridian/ui/<version>/` on every plugin
//! host and on the dashboard's own (spec/plugin-pages-share-one-kit.md, Q2).
//!
//! On a plugin's host it is on the plugin's own origin, so a page links it
//! with nothing reopened of the dashboard's; on the dashboard's, the
//! dashboard's own pages take their tokens from the same stylesheet, so the
//! two read as one product. The kit is meridian-ui's build, carried in the
//! image at a pinned commit (the Dockerfile's `ui` stage): plain files, read
//! from disk as asked for, and nothing else under this path.
//!
//! The version is the deployment's: whichever the image carries. A page that
//! pinned another version of the same major is answered with the newest of
//! that major the image carries -- any `0.x` with the newest `0.x` (the
//! product owner, 2026-09-30) -- so a page built against 0.1.0 has the brand
//! as it is now. Another major, or a path whose first segment is not a
//! version, is answered 404, as any missing file is.
//!
//! It is answered in place, at the address asked for, not redirected there.
//! The kit's references are all relative, and its script finds the rest of
//! the kit from its own `src`, which a redirect does not change: a redirect
//! would cost a round trip for the stylesheet, the script and every component
//! it loads, each of them to land on the same files. In place, the whole kit
//! is read under the version the page named, from one build. The five
//! minutes a file is cached for are the same either way: a new image may
//! answer the same address with a newer build.

use std::path::{Path, PathBuf};

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// Where the kit is, on any host this process serves.
pub const PATH: &str = "/.meridian/ui/";

/// Where the image puts it.
pub const IN_IMAGE: &str = "/usr/share/meridian/ui";

pub struct Kit {
    root: PathBuf,
    /// Every version carried, least first.
    carried: Vec<(Version, String)>,
}

/// A kit version: major, minor, patch.
type Version = [u64; 3];

/// `major.minor.patch`, each a decimal number; anything else is no version.
fn version_of(name: &str) -> Option<Version> {
    let mut parts = name.split('.');
    let mut version = [0; 3];
    for part in &mut version {
        let digits = parts.next()?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *part = digits.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

impl Kit {
    /// The kit under `root`, one directory per version: the greatest one is
    /// the deployment's, which its own pages link.
    pub fn at(root: impl Into<PathBuf>) -> Result<Kit, String> {
        let root = root.into();
        let mut carried: Vec<(Version, String)> = std::fs::read_dir(&root)
            .map_err(|failed| format!("no kit at {}: {failed}", root.display()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("meridian.css").is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| Some((version_of(&name)?, name)))
            .collect();
        if carried.is_empty() {
            return Err(format!("no version of the kit under {}", root.display()));
        }
        carried.sort();
        Ok(Kit { root, carried })
    }

    pub fn version(&self) -> &str {
        &self.carried.last().expect("a kit carries a version").1
    }

    /// Where a page links the kit's stylesheet.
    pub fn stylesheet(&self) -> String {
        format!("{}meridian.css", self.base())
    }

    /// Where the deployment's version of the kit is: its stylesheet, its
    /// script and its components below it.
    pub fn base(&self) -> String {
        format!("{PATH}{}/", self.version())
    }

    /// The version that answers for `asked`: the newest carried of its major.
    fn answering(&self, asked: &str) -> Option<&str> {
        let [major, ..] = version_of(asked)?;
        self.carried
            .iter()
            .rev()
            .find(|(version, _)| version[0] == major)
            .map(|(_, name)| name.as_str())
    }

    /// A file of the kit, for a GET or HEAD of `path` under [`PATH`].
    pub async fn serve(&self, method: &Method, path: &str) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return (StatusCode::METHOD_NOT_ALLOWED, "the kit is read-only\n").into_response();
        }
        let Some(file) = path.strip_prefix(PATH).and_then(|rest| self.file(rest)) else {
            return (StatusCode::NOT_FOUND, "no such file in the kit\n").into_response();
        };
        match tokio::fs::read(&file).await {
            Ok(bytes) => (
                [
                    (CONTENT_TYPE, HeaderValue::from_static(content_type(&file))),
                    // A version's files change only with the image, and a
                    // new image may carry the same version rebuilt, or a
                    // newer one answering it: five minutes, rather than
                    // for ever.
                    (
                        CACHE_CONTROL,
                        HeaderValue::from_static("public, max-age=300"),
                    ),
                    (
                        axum::http::header::X_CONTENT_TYPE_OPTIONS,
                        HeaderValue::from_static("nosniff"),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => (StatusCode::NOT_FOUND, "no such file in the kit\n").into_response(),
        }
    }

    /// A file of the version answering the one `rest` names first, named
    /// below it by segments each of which is a plain name: never `..`, never
    /// empty, never hidden, so nothing outside the kit can be named.
    fn file(&self, rest: &str) -> Option<PathBuf> {
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (asked, below) = rest.split_once('/')?;
        let mut file = self.root.join(self.answering(asked)?);
        for part in below.split('/') {
            if !segment(part) {
                return None;
            }
            file.push(part);
        }
        file.is_file().then_some(file)
    }
}

fn segment(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    /// A kit carrying `versions`, each stylesheet naming its own, with a
    /// component in the greatest alone, a file beside the versions and a
    /// directory like one that is not a version.
    fn carrying(versions: &[&str]) -> (Kit, PathBuf) {
        let root = std::env::temp_dir().join(format!("meridian-kit-{}", crate::session::token()));
        for version in versions.iter().chain(&["latest"]) {
            std::fs::create_dir_all(root.join(version).join("components")).unwrap();
            std::fs::write(root.join(version).join("meridian.css"), *version).unwrap();
        }
        std::fs::write(root.join("secret.txt"), "not the kit's").unwrap();
        let kit = Kit::at(&root).unwrap();
        std::fs::write(
            root.join(kit.version()).join("components/om-grid.js"),
            "export class OmGrid {}",
        )
        .unwrap();
        (kit, root)
    }

    fn kit() -> (Kit, PathBuf) {
        carrying(&["0.1.0", "0.10.0", "0.9.2"])
    }

    /// What `path` is answered with: its status, and its body when found.
    async fn answer(kit: &Kit, path: &str) -> (StatusCode, String) {
        let served = kit.serve(&Method::GET, path).await;
        let status = served.status();
        let body = to_bytes(served.into_body(), 1 << 10).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn the_deployments_version_is_the_greatest_it_carries() {
        let (kit, _) = kit();
        assert_eq!(kit.version(), "0.10.0");
        assert_eq!(kit.stylesheet(), "/.meridian/ui/0.10.0/meridian.css");
        assert!(Kit::at("/nowhere/at/all").is_err());
    }

    #[test]
    fn a_version_is_three_numbers_and_nothing_else() {
        assert_eq!(version_of("0.10.3"), Some([0, 10, 3]));
        assert_eq!(version_of("12.0.0"), Some([12, 0, 0]));
        for not in [
            "",
            "0",
            "0.1",
            "0.1.0.1",
            "v0.1.0",
            "0.1.0-rc1",
            "0.1.",
            ".1.0",
            "0.+1.0",
            "latest",
        ] {
            assert_eq!(version_of(not), None, "{not}");
        }
    }

    #[tokio::test]
    async fn any_version_of_a_major_is_answered_in_place_by_the_newest_carried() {
        let (kit, _) = kit();
        // Any 0.x, carried or not, older or newer, is the newest 0.x.
        for asked in ["0.1.0", "0.2.0", "0.9.2", "0.10.0", "0.99.7"] {
            let (status, body) = answer(&kit, &format!("/.meridian/ui/{asked}/meridian.css")).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, "0.10.0"),
                "{asked}"
            );
        }
        // Below it too: a component the newest has, under the version named.
        let (status, body) = answer(&kit, "/.meridian/ui/0.1.0/components/om-grid.js").await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::OK, "export class OmGrid {}")
        );

        // Another major is not carried; the newest of each that is answers it.
        for asked in ["1.0.0", "2.3.4"] {
            let (status, _) = answer(&kit, &format!("/.meridian/ui/{asked}/meridian.css")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{asked}");
        }
        let (both, _) = carrying(&["0.3.0", "1.0.0", "1.2.0", "0.4.1"]);
        assert_eq!(both.version(), "1.2.0");
        for (asked, answered) in [("1.0.5", "1.2.0"), ("1.9.0", "1.2.0"), ("0.1.0", "0.4.1")] {
            let (status, body) =
                answer(&both, &format!("/.meridian/ui/{asked}/meridian.css")).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, answered),
                "{asked}"
            );
        }
        let (status, _) = answer(&both, "/.meridian/ui/2.0.0/meridian.css").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_file_of_the_kit_is_served_with_its_type_and_nothing_outside_it() {
        let (kit, _) = kit();
        let served = kit
            .serve(&Method::GET, "/.meridian/ui/0.10.0/components/om-grid.js")
            .await;
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(
            served.headers()[CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(served.headers()[CACHE_CONTROL], "public, max-age=300");

        for outside in [
            "/.meridian/ui/secret.txt",
            "/.meridian/ui/latest/meridian.css",
            "/.meridian/ui/0.1/meridian.css",
            "/.meridian/ui/v0.1.0/meridian.css",
            "/.meridian/ui/0.10.0",
            "/.meridian/ui/0.10.0/",
            "/.meridian/ui/0.10.0/../secret.txt",
            "/.meridian/ui/0.1.0/../../secret.txt",
            "/.meridian/ui/0.10.0/%2e%2e/secret.txt",
            "/.meridian/ui/0.10.0//meridian.css",
            "/.meridian/ui/0.10.0/components",
            "/.meridian/ui/0.10.0/components/missing.js",
            "/.meridian/other/0.10.0/meridian.css",
        ] {
            let status = kit.serve(&Method::GET, outside).await.status();
            assert_eq!(status, StatusCode::NOT_FOUND, "{outside}");
        }
        assert_eq!(
            kit.serve(&Method::POST, "/.meridian/ui/0.1.0/meridian.css")
                .await
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}
