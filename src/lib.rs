//! XClip client library.
//!
//! The binary in main.rs is a thin shell around these modules, which keeps
//! the protocol and orchestration logic testable without a desktop session.

pub mod action;
pub mod autostart;
pub mod backend;
pub mod clipboard;
pub mod config;
pub mod feedback;
pub mod hotkey;
pub mod keys;
pub mod net;
pub mod platform;
pub mod tray;