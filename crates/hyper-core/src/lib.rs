pub mod ast;
pub mod audit;
pub mod bench;
pub mod config;
pub mod context_pack;
pub mod diag;
pub mod git;
pub mod jail;
pub mod json_util;
pub mod namer;
pub mod native_capture;
pub mod radar;
pub mod runner;
pub mod search;
pub mod session_index;
pub mod sessions;
pub mod share_card;
pub mod transcript;
pub mod tx_edit;
pub mod update;
pub mod usage;
pub mod wal;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
