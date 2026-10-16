//! Catalog logic for the `ovfs` PostgreSQL extension, independent of
//! PostgreSQL so it can be unit-tested with plain `cargo test`.

pub mod lstat;
pub mod tree;
