//! An instance's own storage claim copied into its shared storage's
//! `shard-0/`, once, before its first launch as several pods (step 3d,
//! checkpoint 5, item 30).
//!
//! The copy and its pods' wait for it are the SDK's `meridian-storage copy`
//! and `meridian-storage copied`, run in the instance's own plugin image (PO
//! items 86 and 88); the launcher makes the copy pod and the init gate, and
//! reads the pod's phase and the gate's exit. What it needs of them is here.

/// Where an instance's pods mount its storage (MERIDIAN_STORAGE_DIR), and
/// where its init gate reads the copy's marker.
pub const STORAGE: &str = "/var/lib/meridian/storage";

/// How long an instance's pod waits for its copy before its init container
/// ends, failed, saying why (review c5c M1); the kubelet starts it again.
/// The SDK's `meridian-storage copied` waits as long; a copy pod Pending
/// longer than this is made again ([`super::copy_pod_now`]).
pub const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
