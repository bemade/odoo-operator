//! Integration tests using envtest — spins up a real API server + etcd.
//!
//! Each submodule tests a specific area of concern. The shared harness and
//! helpers live in `common.rs`.
//!
//! Requirements: Go toolchain + clang (for rust2go/envtest build).
//! Run with: `cargo test --test integration`

mod common;

mod backup_job;
mod bootstrap;
mod child_resources;
mod degraded;
mod finalizer;
mod init_job;
mod restore_job;
mod scaling;
mod upgrade_job;
