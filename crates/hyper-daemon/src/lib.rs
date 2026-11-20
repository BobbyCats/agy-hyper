pub mod client;
pub mod protocol;
pub mod server;
pub mod state;

pub fn daemon_version() -> &'static str {
    hyper_core::version()
}
