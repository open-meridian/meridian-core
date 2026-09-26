//! Generates the domain bindings in `crates/domain/src/v1.rs` from `proto/`.
//!
//! Run through `make codegen`, which runs this inside a container with protoc
//! pinned. It is never run on a host, and its output is never edited by hand.

use std::{fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("../proto");
    // meridian-schema's protos at the revision the workspace pins, for the
    // plugin-facing messages a domain one carries -- the envelope's metadata.
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
    tonic_build::configure()
        .out_dir(&out_dir)
        .build_server(false)
        .build_client(false)
        .extern_path(".meridian.v1.MessageMeta", "::meridian_pb::v1::MessageMeta")
        .compile_protos(&protos, &[proto_root.clone(), schema_root])?;

    // The package stays meridian.v1, so a message's wire name is unchanged by
    // the move out of meridian-schema.
    let generated = out_dir.join("meridian.v1.rs");
    if !generated.exists() {
        return Err(format!("expected {} to exist after generation", generated.display()).into());
    }
    fs::rename(&generated, out_dir.join("v1.rs"))?;
    Ok(())
}
