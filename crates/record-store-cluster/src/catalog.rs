//! Durable cluster catalog.
//!
//! The catalog holds every piece of replicated cluster state: identity,
//! configuration, membership, replica placement, tombstones, movement tasks, and
//! long-running operations. Mutations are applied through transaction-scoped
//! functions so that a consensus state machine can commit a command and its
//! applied log position in one atomic transaction.

mod codec;
mod commands;
mod error;
mod keys;
mod schema;
mod snapshot;
mod store;

#[cfg(test)]
pub(crate) mod test_support;

pub use commands::apply_command_tx;
pub use error::ClusterCatalogError;
pub use schema::{CLUSTER_TABLES, initialize_tables};
pub use snapshot::{export_tx, import_tx, silence};
pub use store::{ClusterCatalog, ClusterUsage, PlacementPage, TaskPage};

pub(crate) use error::{CatalogResult, backend};

/// One raw key/value pair used by consensus snapshots.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    /// Table the pair belongs to.
    pub table: String,
    /// Raw key bytes.
    pub key: Vec<u8>,
    /// Raw value bytes.
    pub value: Vec<u8>,
}
