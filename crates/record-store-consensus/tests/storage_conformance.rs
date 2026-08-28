//! Runs openraft's storage conformance suite against Record Store's own stores.
//!
//! The log and the state machine are the two pieces consensus trusts absolutely:
//! a vote that does not survive a restart allows a double vote, and a truncation
//! that leaves entries behind replays writes the cluster already rejected. Rather
//! than hand-writing those cases, this drives the suite the consensus library
//! itself uses to validate an implementation, so the contract is checked the way
//! its author intended.

use openraft::StorageError;
use openraft::testing::{StoreBuilder, Suite};
use record_store_consensus::{
    MemberId, RecordStoreTypeConfig, RedbLogStore, ReplicatedState, StateMachineStore,
};
use tempfile::TempDir;

/// Builds a fresh log and state machine in a throwaway directory.
struct RecordStoreBuilder;

impl StoreBuilder<RecordStoreTypeConfig, RedbLogStore, StateMachineStore, TempDir>
    for RecordStoreBuilder
{
    async fn build(
        &self,
    ) -> Result<(TempDir, RedbLogStore, StateMachineStore), StorageError<MemberId>> {
        let directory = tempfile::tempdir().expect("temporary directory");
        let log = RedbLogStore::open(directory.path().join("raft.redb"))
            .await
            .expect("open log");
        let state = ReplicatedState::open(
            directory.path().join("state.redb"),
            directory.path().join("snapshots"),
        )
        .await
        .expect("open state");
        Ok((directory, log, StateMachineStore::new(state)))
    }
}

#[test]
fn the_durable_log_and_state_machine_satisfy_the_consensus_storage_contract() {
    Suite::test_all(RecordStoreBuilder).expect("storage conformance");
}
