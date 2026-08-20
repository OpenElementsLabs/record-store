//! Versioned internal wire contracts.
//!
//! No network service is exposed yet. The generated types establish a stable
//! namespace and build pipeline for future node-to-node contracts.

/// Current internal protocol major version.
pub const PROTOCOL_MAJOR_VERSION: u32 = 1;

/// Version 1 node and system messages.
pub mod system_v1 {
    tonic::include_proto!("oes.internal.system.v1");
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::system_v1::NodeDescriptor;

    #[test]
    fn generated_contract_round_trips() {
        let descriptor = NodeDescriptor {
            node_id: "11e3bb32-9c29-42be-b973-1973310287c7".into(),
            protocol_major_version: super::PROTOCOL_MAJOR_VERSION,
        };
        let bytes = descriptor.encode_to_vec();
        assert_eq!(
            NodeDescriptor::decode(bytes.as_slice()).expect("decode descriptor"),
            descriptor
        );
    }
}
