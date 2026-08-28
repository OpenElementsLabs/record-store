use redb::TableDefinition;

pub(crate) const ACCOUNTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("service_accounts.v1");
pub(crate) const ACCESS_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("access_keys.v1");
pub(crate) const ROTATED_CREDENTIALS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("rotated_credentials.v1");
pub(crate) const ACCOUNT_CREDENTIALS: TableDefinition<&[u8], u8> =
    TableDefinition::new("account_credentials.v1");
pub(crate) const POLICIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("policies.v1");
pub(crate) const POLICY_NAMES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("policy_names.v1");
pub(crate) const POLICY_BINDINGS: TableDefinition<&[u8], u8> =
    TableDefinition::new("policy_bindings.v1");
