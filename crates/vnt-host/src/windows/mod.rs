//! Windows service, process and local IPC boundaries.
pub mod agent;
pub mod broker;
pub(crate) mod desktop_effects;
pub(crate) mod display;
pub mod input;
pub mod local;
pub mod management;
pub mod native;
pub mod service;
pub mod store;

#[cfg(test)]
mod tests;
