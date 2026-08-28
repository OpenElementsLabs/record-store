//! Management and public HTTP surfaces for share and embed capabilities.
//!
//! Two surfaces live here and they are deliberately kept apart. The management
//! routes sit under `/api/v1`, behind the same bearer authentication as every
//! other administrative operation, and are where capabilities are created,
//! inspected, and withdrawn. The public routes — `/s/{token}` and `/e/{token}` —
//! carry no session at all: the token in the path *is* the authorization, and it
//! is re-checked against durable state on every single request so that a
//! revocation takes effect on the next one.
//!
//! Nothing on the public surface can reach anything but the one object its
//! capability names, and nothing on it discloses a bucket, a key path, a version
//! identifier, a node, or any other internal fact about how Record Store stores things.

pub(crate) mod dto;
pub(crate) mod embeds;
pub(crate) mod management;
pub(crate) mod public;
pub(crate) mod respond;
pub(crate) mod shares;
pub(crate) mod support;

#[cfg(test)]
mod tests;

pub use management::SharingManagement;

pub(crate) use dto::sharing_settings;
pub(crate) use embeds::{
    create_object_embed, delete_embed, get_embed, get_embed_url, list_object_embeds, revoke_embed,
    update_embed,
};
pub(crate) use management::client_identity;
pub(crate) use public::SHARE_CONTENT_POLICY;
pub(crate) use public::{
    public_embed_content, public_embed_preflight, public_share_content, public_share_descriptor,
    public_share_unlock,
};
pub(crate) use respond::read_probe;
pub(crate) use shares::{
    create_object_share, delete_share, get_share, get_share_url, list_object_shares, revoke_share,
};
