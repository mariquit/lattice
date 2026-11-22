// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! lattice library crate — exposes the CLI's modules for in-process testing.
//!
//! The `lattice` binary (`src/main.rs`) is the product surface; it declares
//! its own module tree and is unaffected by this file. This library re-declares
//! the same modules as `pub` so integration tests can drive internals directly
//! without spawning a child process. The crate-root items referenced through `crate::`
//! (`Result`, `LATTICE_DIR`) are mirrored here so the shared sources compile
//! unchanged in both crates.

pub mod analysis;
pub mod bench;
pub mod branch;
pub mod centrality;
pub mod class;
pub mod context;
pub mod embed;
pub mod envelope;
pub mod freshness;
pub mod fts;
pub mod fuse;
pub mod graph;
pub mod health;
pub mod index;
pub mod ingest;
pub mod install;
pub mod languages;
pub mod leiden;
pub mod markdown;
pub mod meta;
pub mod okf;
pub mod ownership;
pub mod progress;
pub mod review;
pub mod router;
pub mod snapshot;
pub mod vectors;

/// Boxed-error result alias shared across modules (mirrors the binary crate
/// root so `crate::Result` resolves when these sources compile as the library).
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The store directory name (mirrors the binary crate root).
pub const LATTICE_DIR: &str = ".lattice";
