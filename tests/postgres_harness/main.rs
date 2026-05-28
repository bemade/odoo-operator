//! Docker-pg integration tests for `PgPostgresManager`.
//!
//! Run with: `cargo test --test postgres_harness -- --test-threads=1`
//! Requires docker. The container is shared across tests in the binary;
//! tests use unique role names to avoid collisions.

mod harness;

mod delete_role_errors;
mod ensure_role;
