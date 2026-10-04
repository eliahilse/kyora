//! Event-driven terminal frontend. The offline driver is independent of the core runtime.

/// UI state and keyboard actions.
pub mod app;
pub mod demo;
pub mod event;
#[cfg(unix)]
mod signals;
mod terminal;
/// Terminal layout and rendering.
pub mod view;

pub use terminal::run;
