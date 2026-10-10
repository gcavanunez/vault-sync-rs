//! Git synchronization for a notes vault, as a daemon and a command-line client.
//!
//! Pure policy lives in [`model`], [`scheduling`], [`clock`] and the render
//! functions of [`service`]. Everything that touches the system goes through
//! [`process`] (Git and notifications), [`lock`], [`storage`] and [`ipc`].

pub mod clock;
pub mod config;
pub mod control;
pub mod daemon;
pub mod git;
pub mod interrupt;
pub mod ipc;
pub mod lock;
pub mod log;
pub mod model;
pub mod notifications;
pub mod process;
pub mod scheduling;
pub mod service;
pub mod storage;
pub mod sync;
