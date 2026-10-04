//! Event-driven terminal frontend. The offline driver is independent of the core runtime.

pub mod app;
pub mod demo;
pub mod event;
mod terminal;
pub mod view;

pub use terminal::run;
