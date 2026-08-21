//! Versioned internal wire contracts.
//!
//! Only generated types live here. The service implementations and clients live
//! in `oes-rpc`, which keeps the contract crate free of transport, storage, and
//! consensus dependencies.

/// Current internal protocol major version.
///
/// The authoritative compatibility rules live in `oes-cluster`; this constant
/// exists so the generated contracts and those rules cannot drift apart.
pub const PROTOCOL_MAJOR_VERSION: u32 = 1;

/// Version 1 node identity and lifecycle messages.
pub mod system_v1 {
    tonic::include_proto!("oes.internal.system.v1");
}

/// Version 1 metadata consensus transport.
pub mod consensus_v1 {
    tonic::include_proto!("oes.internal.consensus.v1");
}

/// Version 1 replica transfer and integrity operations.
pub mod replica_v1 {
    tonic::include_proto!("oes.internal.replica.v1");
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::system_v1::NodeDescriptor;

    #[test]
    fn generated_contract_round_trips() {
        let descriptor = NodeDescriptor {
            node_id: "11e3bb32-9c29-42be-b973-1973310287c7".into(),
            member_id: 3,
            protocol_major_version: super::PROTOCOL_MAJOR_VERSION,
            protocol_minor_version: 1,
            software_version: "0.1.0".into(),
            storage_format_version: 1,
            cluster_format_version: 1,
            cluster_id: "5cf6a45f-9e6f-4c58-9f2f-f4b4b56f2f92".into(),
            rpc_address: "10.0.1.12:7603".into(),
            storage_node: true,
        };
        let bytes = descriptor.encode_to_vec();
        assert_eq!(
            NodeDescriptor::decode(bytes.as_slice()).expect("decode descriptor"),
            descriptor
        );
    }

    #[test]
    fn malformed_payloads_are_rejected_without_panicking() {
        // Internal traffic is not trusted: a malformed frame must produce an
        // error, never a panic on a node.
        for payload in [
            vec![0xff_u8; 32],
            vec![0x08],
            vec![0x0a, 0xff, 0xff, 0xff, 0x7f],
            b"not protobuf at all".to_vec(),
        ] {
            let _ = NodeDescriptor::decode(payload.as_slice());
        }
    }
}
