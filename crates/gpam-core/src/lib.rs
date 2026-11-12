//! Frontend-agnostic core of gpam, a client for Google Cloud Privileged
//! Access Manager. Holds the PAM and Resource Manager backends, the local
//! SQLite cache, background refresh and grant polling, the approval socket,
//! and the view state that frontends render.

pub mod app;
pub(crate) mod auth;
pub mod backend;
pub mod cache;
mod fuzzy;
pub mod gcp;
pub mod ipc;
pub mod logs;
pub mod poller;
pub mod refresh;
