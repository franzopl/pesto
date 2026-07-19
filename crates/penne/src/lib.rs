//! Public API of `penne`, a fast NZB downloader for Usenet.
//!
//! `penne` reads a `.nzb` file, fetches its articles over NNTP, reassembles
//! them into the original files, and verifies/repairs the result with PAR2.
//! See `ROADMAP.md` for the development plan and current phase.
//!
//! `penne` reuses `pesto`'s NNTP connection (TLS + `AUTHINFO`) and `.nzb`
//! parser rather than duplicating that logic — see [`pesto::nntp`] and
//! [`pesto::nzb::parse`].
//!
//! This crate is the download engine underneath the future `penne` CLI and,
//! eventually, a web UI (à la SABnzbd) built on top of it. The web UI is out
//! of scope until the CLI reaches feature parity with the roadmap.

pub mod assemble;
pub mod cache;
pub mod check;
pub mod client;
pub mod config;
pub mod deobfuscate;
pub mod diskspace;
pub mod download;
pub mod extract;
pub mod health;
pub mod nzb;
pub mod progress;
pub mod queue;
pub mod quickcheck;
pub mod repair;
pub mod ui;
pub mod wizard;
