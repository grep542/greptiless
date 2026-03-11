use thiserror::Error;

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("Identity check failed for wallet {wallet}: {reason}")]
    IdentityCheckFailed { wallet: String, reason: String },

    #[error("Keyring API error (status {status}): {message}")]
    KeyringApiError { status: u16, message: String },

    #[error("Wallet {wallet} is blacklisted under policy {policy_id}")]
    WalletBlacklisted { wallet: String, policy_id: u32 },

    #[error("No yield opportunities found for chain {chain} with min APY {min_apy}")]
    NoOpportunitiesFound { chain: String, min_apy: String },

    #[error("Protocol data fetch failed for {protocol}: {reason}")]
    ProtocolFetchFailed { protocol: String, reason: String },

    #[error("No compliant opportunities found after filtering {total} candidates")]
    NoCompliantOpportunities { total: usize },

    #[error("Capital amount ${amount} is below minimum required ${minimum}")]
    InsufficientCapital { amount: String, minimum: String },

    #[error("HTTP request error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Request timed out after {secs}s calling {endpoint}")]
    Timeout { secs: u64, endpoint: String },

    #[error("JSON deserialization error: {0}")]
    DeserializationError(#[from] serde_json::Error),

    #[error("Ethereum interaction error: {0}")]
    EthereumError(String),

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("Internal SDK error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, RouterError>;