//! Generates the domain bindings in `crates/domain/src/v1.rs` from `proto/`.
//!
//! Run through `make codegen`, which runs this inside a container with protoc
//! pinned. It is never run on a host, and its output is never edited by hand.

use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("../proto");
    // meridian-schema's protos at the revision the workspace pins, for the
    // plugin-facing messages a domain one carries -- the envelope's metadata,
    // and a plugin's setting declarations.
    // On the include path only: those are generated there, and referred to here.
    let schema_root = PathBuf::from("/schema-proto");
    let out_dir = PathBuf::from("/out");
    fs::create_dir_all(&out_dir)?;

    let mut protos: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(proto_root.join("meridian/v1"))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("proto") {
            protos.push(path);
        }
    }
    protos.sort();

    // The domain files declare no service: the runtime carries them as
    // payloads over the bus, and the only gRPC surface is the sidecar's, which
    // meridian-schema generates. No extra derives, for the reason given there.
    let mut config = tonic_build::configure()
        .out_dir(&out_dir)
        .build_server(false)
        .build_client(false);

    // Every type meridian-schema declares in this package is referred to, never
    // generated again: prost generates the whole package, imports included, so
    // a schema message named nowhere here would come out as a second type of
    // the same name. The envelope's metadata, and a plugin's setting
    // declarations (W4.8, W6.11), are what a domain message carries today.
    for name in schema_types(&schema_root.join("meridian/v1"))? {
        config = config.extern_path(
            format!(".meridian.v1.{name}"),
            format!("::meridian_pb::v1::{name}"),
        );
    }

    config.compile_protos(&protos, &[proto_root.clone(), schema_root])?;

    // The package stays meridian.v1, so a message's wire name is unchanged by
    // the move out of meridian-schema.
    let generated = out_dir.join("meridian.v1.rs");
    if !generated.exists() {
        return Err(format!("expected {} to exist after generation", generated.display()).into());
    }
    fs::rename(&generated, out_dir.join("v1.rs"))?;
    Ok(())
}

/// The top-level messages and enums the protos in `dir` declare, by name.
/// A nested type is covered by its parent's path.
fn schema_types(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("proto") {
            continue;
        }
        for line in fs::read_to_string(&path)?.lines() {
            let declared = line
                .strip_prefix("message ")
                .or_else(|| line.strip_prefix("enum "));
            if let Some(name) = declared.and_then(|rest| rest.split_whitespace().next()) {
                names.push(name.trim_end_matches('{').to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}
