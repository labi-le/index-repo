pub mod chroma;
pub mod chunk;
pub mod chunkfile;
pub mod config;
pub mod daemon;
pub mod embed;
pub mod grammar;
pub mod lazy;
pub mod oneshot;
pub mod registry;
pub mod service;
pub mod splitlines;
pub mod store;
pub mod walk;

#[cfg(test)]
pub(crate) mod testkit;
