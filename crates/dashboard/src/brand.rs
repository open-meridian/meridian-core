//! The Open Meridian icon, as a browser's tab and a phone's home screen show
//! it (the product owner, 2026-10-06: the icon on the deployment too).
//!
//! The four files every surface serves (meridian-design's brand/README.md):
//! `favicon-32.png` and `favicon.svg` as the tab's icon -- the SVG adaptive,
//! indigo, soft indigo when the browser is dark, linked last so a browser
//! that reads SVG takes it -- `apple-touch-icon.png` on its own white ground,
//! and `favicon.ico`, which browsers and feed readers ask for unprompted.
//! Copied from meridian-design's `brand/` (da6b3dd) into the binary, so the
//! dashboard serves them with nothing read from disk: on its own host, and on
//! every plugin's host, where the pages the dashboard draws link the same
//! paths, before anything about who asks, as the kit is
//! ([`crate::kit`]).

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// The icon a page's head links, each path once, in the order linked.
pub const LINKS: &str =
    "<link rel=\"icon\" href=\"/favicon-32.png\" sizes=\"32x32\" type=\"image/png\">\
     <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\
     <link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">";

/// Each file by the path it is served at, with its type.
const FILES: [(&str, &str, &[u8]); 4] = [
    (
        "/favicon.svg",
        "image/svg+xml",
        include_bytes!("../brand/favicon.svg"),
    ),
    (
        "/favicon-32.png",
        "image/png",
        include_bytes!("../brand/favicon-32.png"),
    ),
    (
        "/favicon.ico",
        "image/x-icon",
        include_bytes!("../brand/favicon.ico"),
    ),
    (
        "/apple-touch-icon.png",
        "image/png",
        include_bytes!("../brand/apple-touch-icon.png"),
    ),
];

/// Whether `path` is one of the icon's, which this answers on any host.
pub fn is_icon(path: &str) -> bool {
    FILES.iter().any(|(at, _, _)| *at == path)
}

/// The file at `path`, for a GET or HEAD; 404 for any other path, 405 for
/// any other method.
pub fn serve(method: &Method, path: &str) -> Response {
    let Some((_, kind, bytes)) = FILES.iter().find(|(at, _, _)| *at == path) else {
        return (StatusCode::NOT_FOUND, "no such icon\n").into_response();
    };
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "the icon is read-only\n").into_response();
    }
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(kind)),
            // A new image may carry a new icon: a day, rather than for ever.
            (
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            ),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ],
        *bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn each_file_is_answered_whole_with_its_type_and_nothing_else_is() {
        for (path, kind, first) in [
            ("/favicon.svg", "image/svg+xml", &b"<svg"[..]),
            ("/favicon-32.png", "image/png", &b"\x89PNG"[..]),
            ("/favicon.ico", "image/x-icon", &b"\x00\x00\x01\x00"[..]),
            ("/apple-touch-icon.png", "image/png", &b"\x89PNG"[..]),
        ] {
            assert!(is_icon(path));
            let answer = serve(&Method::GET, path);
            assert_eq!(answer.status(), StatusCode::OK, "{path}");
            assert_eq!(answer.headers()[CONTENT_TYPE], kind, "{path}");
            assert!(answer.headers()[CACHE_CONTROL]
                .to_str()
                .unwrap()
                .starts_with("public"));
            let body = to_bytes(answer.into_body(), 1 << 20).await.unwrap();
            assert!(body.starts_with(first), "{path} is not what it says it is");
        }
        let svg = to_bytes(serve(&Method::GET, "/favicon.svg").into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&svg)
                .unwrap()
                .contains("prefers-color-scheme:dark"),
            "the tab's icon adapts to a dark browser"
        );
        for path in ["/favicon.png", "/favicon.svg/", "/brand/favicon.svg", "/"] {
            assert!(!is_icon(path));
            assert_eq!(serve(&Method::GET, path).status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(
            serve(&Method::POST, "/favicon.ico").status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn the_links_name_the_png_then_the_svg_then_the_touch_icon() {
        let png = LINKS.find("/favicon-32.png").unwrap();
        let svg = LINKS.find("/favicon.svg").unwrap();
        let touch = LINKS.find("/apple-touch-icon.png").unwrap();
        assert!(png < svg && svg < touch);
        assert!(LINKS.contains("sizes=\"32x32\"") && LINKS.contains("type=\"image/svg+xml\""));
        assert!(LINKS.contains("rel=\"apple-touch-icon\""));
    }
}
