//! sill core: everything the shelf and the CLI share and that has no UI.
//!
//! - [`policy`]  — what a session may see, write and reach (TOML, SPEC §4.2 / §4.9)
//! - [`mounts`]  — the host's mount table, the basis for the per-directory overlays
//! - [`jail`]    — turns a policy + session into a bubblewrap command line (SPEC §4.2)
//! - [`paths`]   — `~` expansion, globs, the escaped names used for upper trees

pub mod acp;
pub mod agents;
pub mod changes;
pub mod jail;
pub mod keep;
pub mod ledger;
pub mod mounts;
pub mod paths;
pub mod policy;
pub mod proxy;
pub mod session;
pub mod shim;
pub mod store;
pub mod title;
