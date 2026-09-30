//! An older plugin's admin pages, read as pages at `admin` (W4.8).
//!
//! Contract v5 declares a plugin's pages in one list, each with the levels it
//! serves, and retires the list of admin pages: `InterfaceDeclaration` field 3
//! is reserved, so the generated types no longer have it, and decoding drops
//! it without a word (sdk-contract/a-plugin-has-admins). While this sidecar
//! accepts a plugin built against an older contract, it reads that plugin's
//! admin pages as pages at `admin`, in the order declared, so a plugin built
//! before keeps its pages.
//!
//! Read from the request's bytes, before the generated service decodes them,
//! because the field is gone from the type by then: each admin page (field 3)
//! is written again as a page (field 4) with one level, `admin`. Nothing is
//! done for a plugin declaring v5 or later, whose SDK has no admin pages to
//! send, nor for a request this cannot read, which the service then refuses
//! as it would have.

use std::convert::Infallible;
use std::task::{Context, Poll};

use meridian_pb::v1::AccessLevel;
use tonic::body::Body;
use tonic::codegen::{http, BoxFuture, Service};
use tonic::server::NamedService;

/// The path Register is called at.
const REGISTER: &str = "/meridian.v1.SidecarService/Register";

/// The first contract whose plugins declare pages with levels.
const PAGES_FROM: u32 = 5;

/// A registration is a few kilobytes; one past this is not read here.
const MOST: usize = 1 << 20;

// RegisterRequest's and InterfaceDeclaration's field numbers.
const SCHEMA_VERSION: u32 = 4;
const INTERFACE: u32 = 5;
const ADMIN_PAGES: u32 = 3;
const PAGES: u32 = 4;
const LEVELS: u32 = 3;

const VARINT: u64 = 0;
const LENGTH_DELIMITED: u64 = 2;

/// Wraps the generated sidecar service, rewriting Register's request.
#[derive(Clone)]
pub struct OlderPagesRead<S> {
    inner: S,
}

impl<S> OlderPagesRead<S> {
    pub fn new(inner: S) -> Self {
        OlderPagesRead { inner }
    }
}

impl<S: NamedService> NamedService for OlderPagesRead<S> {
    const NAME: &'static str = S::NAME;
}

impl<S> Service<http::Request<Body>> for OlderPagesRead<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        if request.uri().path() != REGISTER {
            return Box::pin(self.inner.call(request));
        }
        // The one polled ready is the one called; a clone takes its place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(axum::body::Body::new(body), MOST)
                .await
                .unwrap_or_default();
            let body = framed(&bytes).unwrap_or_else(|| bytes.to_vec());
            let request = http::Request::from_parts(parts, Body::new(axum::body::Body::from(body)));
            inner.call(request).await
        })
    }
}

/// A gRPC body of one message, rewritten, or None to leave it as it came: a
/// compressed or malformed frame, or nothing to rewrite.
fn framed(body: &[u8]) -> Option<Vec<u8>> {
    let (&compressed, rest) = body.split_first()?;
    let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let message = rest.get(4..4 + length)?;
    if compressed != 0 || rest.len() != 4 + length {
        return None;
    }
    let rewritten = admin_pages_as_pages(message)?;
    let mut out = Vec::with_capacity(5 + rewritten.len());
    out.push(0);
    out.extend_from_slice(&(rewritten.len() as u32).to_be_bytes());
    out.extend_from_slice(&rewritten);
    Some(out)
}

/// A serialised RegisterRequest from a plugin built before v5, with each of
/// its admin pages written as a page at `admin`; None when there is nothing
/// to rewrite, or the bytes do not read.
pub fn admin_pages_as_pages(request: &[u8]) -> Option<Vec<u8>> {
    let fields = wire::fields(request)?;
    let version = fields
        .iter()
        .find(|field| field.number == SCHEMA_VERSION)
        .and_then(|field| field.value)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|text| text.strip_prefix('v')?.parse::<u32>().ok())?;
    if version >= PAGES_FROM {
        return None;
    }
    let mut changed = false;
    let mut out = Vec::with_capacity(request.len() + 16);
    for field in &fields {
        match (field.number, field.value) {
            (INTERFACE, Some(interface)) => {
                let rewritten = interface_rewritten(interface)?;
                changed |= rewritten.as_slice() != interface;
                wire::put(&mut out, INTERFACE, &rewritten);
            }
            _ => out.extend_from_slice(field.raw),
        }
    }
    changed.then_some(out)
}

fn interface_rewritten(interface: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(interface.len() + 8);
    for field in wire::fields(interface)? {
        match (field.number, field.value) {
            (ADMIN_PAGES, Some(page)) => {
                let mut at_admin = page.to_vec();
                prost::encoding::encode_varint(u64::from(LEVELS) << 3 | VARINT, &mut at_admin);
                prost::encoding::encode_varint(AccessLevel::Admin as u64, &mut at_admin);
                wire::put(&mut out, PAGES, &at_admin);
            }
            _ => out.extend_from_slice(field.raw),
        }
    }
    Some(out)
}

/// Just enough of the protobuf wire format to walk a message's fields.
mod wire {
    use super::LENGTH_DELIMITED;

    pub struct Field<'a> {
        pub number: u32,
        /// A length-delimited field's bytes.
        pub value: Option<&'a [u8]>,
        /// The whole field, key included, as it came.
        pub raw: &'a [u8],
    }

    pub fn fields(message: &[u8]) -> Option<Vec<Field<'_>>> {
        let mut fields = Vec::new();
        let mut rest = message;
        while !rest.is_empty() {
            let start = rest;
            let key = prost::encoding::decode_varint(&mut rest).ok()?;
            let number = u32::try_from(key >> 3).ok()?;
            let value = match key & 7 {
                0 => {
                    prost::encoding::decode_varint(&mut rest).ok()?;
                    None
                }
                1 => {
                    rest = rest.get(8..)?;
                    None
                }
                2 => {
                    let length =
                        usize::try_from(prost::encoding::decode_varint(&mut rest).ok()?).ok()?;
                    let value = rest.get(..length)?;
                    rest = &rest[length..];
                    Some(value)
                }
                5 => {
                    rest = rest.get(4..)?;
                    None
                }
                _ => return None,
            };
            let raw = &start[..start.len() - rest.len()];
            fields.push(Field { number, value, raw });
        }
        Some(fields)
    }

    /// Append a length-delimited field.
    pub fn put(out: &mut Vec<u8>, number: u32, value: &[u8]) {
        prost::encoding::encode_varint(u64::from(number) << 3 | LENGTH_DELIMITED, out);
        prost::encoding::encode_varint(value.len() as u64, out);
        out.extend_from_slice(value);
    }
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::{InterfaceDeclaration, PageDeclaration, RegisterRequest};
    use prost::Message;

    use super::*;

    /// What a plugin built before v5 sends: its admin pages as field 3.
    fn older(version: &str, pages: &[(&str, &str)]) -> Vec<u8> {
        let mut interface = Vec::new();
        prost::encoding::encode_varint(1 << 3, &mut interface);
        prost::encoding::encode_varint(8000, &mut interface);
        for (path, title) in pages {
            let page = PageDeclaration {
                path: path.to_string(),
                title: title.to_string(),
                levels: vec![],
            };
            wire::put(&mut interface, ADMIN_PAGES, &page.encode_to_vec());
        }
        let mut request = RegisterRequest {
            schema_version: version.into(),
            ..Default::default()
        }
        .encode_to_vec();
        wire::put(&mut request, INTERFACE, &interface);
        request
    }

    #[test]
    fn an_older_plugins_admin_pages_are_read_as_pages_at_admin_in_order() {
        let rewritten = admin_pages_as_pages(&older(
            "v4",
            &[
                ("/admin/connections", "Connections"),
                ("/admin/accounts", "Accounts"),
            ],
        ))
        .expect("rewritten");
        let request = RegisterRequest::decode(rewritten.as_slice()).unwrap();
        let interface: InterfaceDeclaration = request.interface.unwrap();
        assert_eq!(interface.loopback_port, 8000);
        let pages: Vec<_> = interface
            .pages
            .iter()
            .map(|p| (p.path.as_str(), p.title.as_str(), p.levels.clone()))
            .collect();
        assert_eq!(
            pages,
            [
                (
                    "/admin/connections",
                    "Connections",
                    vec![AccessLevel::Admin as i32]
                ),
                (
                    "/admin/accounts",
                    "Accounts",
                    vec![AccessLevel::Admin as i32]
                ),
            ]
        );
        assert_eq!(request.schema_version, "v4");
    }

    #[test]
    fn a_plugin_built_for_v5_is_left_as_it_came() {
        assert_eq!(
            admin_pages_as_pages(&older("v5", &[("/admin", "Admin")])),
            None
        );
        assert_eq!(
            admin_pages_as_pages(&older("v4", &[])),
            None,
            "nothing to read"
        );
    }

    #[test]
    fn what_does_not_read_is_left_to_the_service_to_refuse() {
        assert_eq!(admin_pages_as_pages(&[0x0a, 0x05]), None);
        assert_eq!(framed(&[1, 0, 0, 0, 0]), None, "compressed");
    }

    #[test]
    fn a_frame_is_rewritten_whole() {
        let message = older("v3", &[("/admin", "Admin")]);
        let mut body = vec![0];
        body.extend_from_slice(&(message.len() as u32).to_be_bytes());
        body.extend_from_slice(&message);
        let out = framed(&body).expect("rewritten");
        let length = u32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
        assert_eq!(out.len(), 5 + length);
        let request = RegisterRequest::decode(&out[5..]).unwrap();
        assert_eq!(request.interface.unwrap().pages.len(), 1);
    }
}
