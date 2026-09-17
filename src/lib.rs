#[cfg(not(target_os = "linux"))]
compile_error!("logishell currently supports Linux only");

pub mod bluetooth;
pub mod cli;
pub mod config;
pub mod device;
pub mod hidpp;
pub mod model;
pub mod remap;
pub mod runtime;
pub mod terminal;
mod wizard;
