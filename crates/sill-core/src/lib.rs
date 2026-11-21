//! sill core: everything the daemon and the CLI share and that has no UI.
//!
//! - [`policy`]  — what a session may see, write and reach (TOML, SPEC §4.2 / §4.9)
//! - [`mounts`]  — the host's mount table, the basis for the per-directory overlays
//! - [`jail`]    — turns a policy + session into a bubblewrap command line (SPEC §4.2)
//! - [`launcher`] — how a session process is started, limited and stopped: unit or direct (SPEC §4.7)
//! - [`paths`]   — `~` expansion, globs, the escaped names used for upper trees

pub mod acp;
pub mod agent_storage;
pub mod agents;
pub mod bus;
pub mod changes;
pub mod config;
pub mod credentials;
pub mod crosscheck;
pub mod desktop;
pub mod desktop_capture;
pub mod frontend;
pub mod gpu;
pub mod history;
pub mod instructions;
pub mod jail;
pub mod keep;
pub mod launcher;
pub mod ledger;
pub mod login;
pub mod memory;
pub mod mounts;
pub mod paths;
pub mod permissions;
pub mod policy;
pub mod providers;
pub mod proxy;
pub mod registry;
pub mod safe_fs;
pub mod seccomp;
pub mod session;
pub mod sfp;
pub mod sfp_client;
pub mod shim;
pub mod staged_pin;
pub mod store;
pub mod title;
pub mod wire;
pub mod workspace;
pub mod write_fence;

pub mod operations;

pub mod acp_v2;

pub mod storage;
