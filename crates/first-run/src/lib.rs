//! First run: what a deployment's wizard decides, and the narrow rights that
//! write it.
//!
//! The dashboard asks and this acts (decisions/016). It holds the only right
//! in the deployment to change the cluster, it is limited to resources the
//! chart names, and it gives that right up when the configuration is applied.
//!
//! - [`sealing`]: how a credential crosses the bus without the bus seeing it.
//! - [`cluster`]: the handful of Kubernetes calls it is allowed to make.

pub mod cluster;
pub mod sealing;
pub mod service;

pub use sealing::{seal, SealingKey};
pub use service::{
    BroughtServer, DatabaseProbe, DirectoryProbe, FirstRun, Names, NewRole, Provision, Provisioner,
};
