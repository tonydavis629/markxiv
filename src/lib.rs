// async_trait expands to #[must_use] on futures, which newer clippy flags.
#![allow(clippy::double_must_use)]

pub mod arxiv;
pub mod cache;
pub mod convert;
pub mod disk_cache;
pub mod routes;
pub mod state;
pub mod tex_main;
