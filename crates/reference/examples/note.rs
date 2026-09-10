//! Generate a key, or sign a note with one. For proving interoperability by
//! hand against a running platform.
//!
//! Two implementations of a signing scheme agreeing is not something to assume.
//! This exists so the agreement can be demonstrated rather than argued.
//!
//!     cargo run --example note                       # a new keypair
//!     cargo run --example note <key.pem> <dep> <aud> # a note signed with it

use std::time::{SystemTime, UNIX_EPOCH};

use meridian_reference::DeploymentKey;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        let key = DeploymentKey::generate();
        // Printed only by this example, which exists for a local demonstration.
        // Nothing in the crate itself ever writes a private key anywhere.
        println!("{}", key.private_key_pem().expect("export"));
        println!("{}", key.public_key_pem().expect("export"));
        return;
    }

    let pem = std::fs::read_to_string(&args[0]).expect("read the key");
    let key = DeploymentKey::from_pkcs8_pem(&pem).expect("load the key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    println!("{}", key.note(&args[1], &args[2], now, 45).expect("sign"));
}
