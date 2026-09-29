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
//! The version is the deployment's: whichever the image carries. A page
//! that pinned another is answered 404, as any missing file is.

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
    version: String,
}

impl Kit {
    /// The kit under `root`, one directory per version: the greatest one is
    /// the deployment's, which its own pages link.
    pub fn at(root: impl Into<PathBuf>) -> Result<Kit, String> {
        let root = root.into();
        let mut versions: Vec<String> = std::fs::read_dir(&root)
            .map_err(|failed| format!("no kit at {}: {failed}", root.display()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("meridian.css").is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| segment(name))
            .collect();
        versions.sort_by_key(|version| {
            version
                .split('.')
                .map(|part| part.parse::<u64>().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        let version = versions
            .pop()
            .ok_or_else(|| format!("no version of the kit under {}", root.display()))?;
        Ok(Kit { root, version })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// Where a page links the kit's stylesheet.
    pub fn stylesheet(&self) -> String {
        format!("{PATH}{}/meridian.css", self.version)
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
                    // new image may carry the same version rebuilt: five
                    // minutes, rather than for ever.
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

    /// A file below the root, named by segments each of which is a plain
    /// name: never `..`, never empty, never hidden, so nothing outside the
    /// kit can be named.
    fn file(&self, rest: &str) -> Option<PathBuf> {
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let mut file = self.root.clone();
        let mut depth = 0;
        for part in rest.split('/') {
            if !segment(part) {
                return None;
            }
            file.push(part);
            depth += 1;
        }
        (depth >= 2 && file.is_file()).then_some(file)
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

    fn kit() -> (Kit, PathBuf) {
        let root = std::env::temp_dir().join(format!("meridian-kit-{}", crate::session::token()));
        for version in ["0.1.0", "0.10.0", "0.9.2"] {
            std::fs::create_dir_all(root.join(version).join("components")).unwrap();
            std::fs::write(root.join(version).join("meridian.css"), ":root{}").unwrap();
        }
        std::fs::write(
            root.join("0.10.0/components/om-grid.js"),
            "export class OmGrid {}",
        )
        .unwrap();
        std::fs::write(root.join("secret.txt"), "not the kit's").unwrap();
        (Kit::at(&root).unwrap(), root)
    }

    #[test]
    fn the_deployments_version_is_the_greatest_it_carries() {
        let (kit, _) = kit();
        assert_eq!(kit.version(), "0.10.0");
        assert_eq!(kit.stylesheet(), "/.meridian/ui/0.10.0/meridian.css");
        assert!(Kit::at("/nowhere/at/all").is_err());
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
        let body = to_bytes(served.into_body(), 1 << 10).await.unwrap();
        assert_eq!(&body[..], b"export class OmGrid {}");

        for outside in [
            "/.meridian/ui/secret.txt",
            "/.meridian/ui/0.10.0/../secret.txt",
            "/.meridian/ui/0.10.0/%2e%2e/secret.txt",
            "/.meridian/ui/0.10.0//meridian.css",
            "/.meridian/ui/0.10.0/components",
            "/.meridian/ui/0.2.0/meridian.css",
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
