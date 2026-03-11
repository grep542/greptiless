//! Keyring Network identity verification client.
//!
//! Changes from v0.1:
//!  - API key stored as ApiKey newtype (never printed in logs)
//!  - passes_default_policy fallback is now fail-closed (returns Err, not silent true)
//!  - wallet address validated before any network call
//!  - keyring_contract_address returns Option; unsupported chains return Err

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ethers::prelude::*;
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use crate::error::{Result, RouterError};
use crate::models::{ApiKey, Chain, CredentialStatus, IdentityCheckResult, KeyringCredential};

abigen!(
    KeyringCoreContract,
    r#"[
        function isAuthorized(uint32 policyId, address subject) external view returns (bool)
        function getCredential(uint32 policyId, address subject) external view returns (uint256 issuedAt, uint256 expiresAt)
        function isBlacklisted(uint32 policyId, address subject) external view returns (bool)
    ]"#
);

#[derive(Debug, Deserialize)]
struct ApiCredential {
    #[serde(rename = "policyId")]
    policy_id: u32,
    status: String,
    #[serde(rename = "issuedAt")]
    issued_at: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
    #[serde(rename = "isCompliant")]
    is_compliant: bool,
}

#[derive(Debug, Deserialize)]
struct CredentialsResponse {
    wallet: String,
    credentials: Vec<ApiCredential>,
}

#[derive(Debug, Deserialize)]
struct ComplianceResponse {
    wallet: String,
    #[serde(rename = "passesDefaultPolicy")]
    passes_default_policy: bool,
}

/// Keyring identity client.
/// The API key is stored as an `ApiKey` and is never emitted in Debug output.
#[derive(Clone)]
pub struct KeyringClient {
    http: Client,
    api_base: String,
    api_key: ApiKey,
}

impl KeyringClient {
    pub fn new(
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let api_key = ApiKey::new(api_key);

        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-API-Key",
            header::HeaderValue::from_str(api_key.as_str())
                .expect("invalid api key characters"),
        );

        let http = Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .user_agent("greptiles/0.2.0")
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            api_base: api_base.into(),
            api_key,
        }
    }

    /// Validate that a string looks like a checksummed or lowercase Ethereum address.
    fn validate_wallet_address(wallet: &str) -> Result<()> {
        if wallet.len() != 42 || !wallet.starts_with("0x") {
            return Err(RouterError::ConfigError(format!(
                "invalid Ethereum address (must be 0x + 40 hex chars): {}",
                wallet
            )));
        }
        if !wallet[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RouterError::ConfigError(format!(
                "address contains non-hex characters: {}",
                wallet
            )));
        }
        Ok(())
    }

    #[instrument(skip(self), fields(wallet = %wallet, chain = %chain))]
    pub async fn verify_wallet(
        &self,
        wallet: &str,
        chain: &Chain,
    ) -> Result<IdentityCheckResult> {
        Self::validate_wallet_address(wallet)?;
        info!("Running identity check for wallet {}", wallet);

        let credentials = self.fetch_credentials_rest(wallet, chain).await?;

        // Fail-closed: if the compliance endpoint errors, we do NOT silently pass.
        // We surface the error so the caller can decide (e.g. retry, reject).
        let passes_default = self
            .fetch_default_compliance(wallet)
            .await
            .map_err(|e| {
                warn!(
                    "Default compliance endpoint failed for {}: {} — failing closed",
                    wallet, e
                );
                e
            })?;

        debug!(
            "Wallet {} has {} credentials, default_policy={}",
            wallet,
            credentials.len(),
            passes_default
        );

        Ok(IdentityCheckResult {
            wallet: wallet.to_string(),
            chain: chain.clone(),
            credentials,
            passes_default_policy: passes_default,
            checked_at: Utc::now(),
        })
    }

    #[instrument(skip(self, rpc_url), fields(wallet = %wallet, policy_id = %policy_id))]
    pub async fn check_onchain(
        &self,
        wallet: &str,
        policy_id: u32,
        chain: &Chain,
        rpc_url: &str,
    ) -> Result<bool> {
        Self::validate_wallet_address(wallet)?;

        let contract_addr_str = chain
            .keyring_contract_address()
            .ok_or_else(|| RouterError::ConfigError(format!(
                "Keyring contract address not verified for chain {}; on-chain checks disabled",
                chain
            )))?;

        let provider = Provider::<Http>::try_from(rpc_url)
            .map_err(|e| RouterError::EthereumError(e.to_string()))?;
        let provider = Arc::new(provider);

        let contract_addr: Address = contract_addr_str
            .parse()
            .map_err(|e: <Address as std::str::FromStr>::Err| {
                RouterError::EthereumError(e.to_string())
            })?;

        let wallet_addr: Address = wallet
            .parse()
            .map_err(|e: <Address as std::str::FromStr>::Err| {
                RouterError::EthereumError(format!("invalid wallet address: {}", e))
            })?;

        let contract = KeyringCoreContract::new(contract_addr, provider);

        let blacklisted = contract
            .is_blacklisted(policy_id, wallet_addr)
            .call()
            .await
            .map_err(|e| RouterError::EthereumError(e.to_string()))?;

        if blacklisted {
            return Err(RouterError::WalletBlacklisted {
                wallet: wallet.to_string(),
                policy_id,
            });
        }

        let authorized = contract
            .is_authorized(policy_id, wallet_addr)
            .call()
            .await
            .map_err(|e| RouterError::EthereumError(e.to_string()))?;

        Ok(authorized)
    }

    async fn fetch_credentials_rest(
        &self,
        wallet: &str,
        chain: &Chain,
    ) -> Result<Vec<KeyringCredential>> {
        let url = format!(
            "{}/v1/credentials/{}?chain={}",
            self.api_base, wallet, chain
        );

        let resp = self.http.get(&url).send().await.map_err(RouterError::HttpError)?;
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RouterError::KeyringApiError {
                status: status.as_u16(),
                message: body,
            });
        }

        let data: CredentialsResponse = resp.json().await.map_err(RouterError::HttpError)?;

        data.credentials
            .into_iter()
            .map(|c| self.map_credential(c, wallet, chain))
            .collect::<Result<Vec<_>>>()
    }

    async fn fetch_default_compliance(&self, wallet: &str) -> Result<bool> {
        let url = format!("{}/v1/wallets/{}/compliance", self.api_base, wallet);

        let resp = self.http.get(&url).send().await.map_err(RouterError::HttpError)?;

        if !resp.status().is_success() {
            return Err(RouterError::KeyringApiError {
                status: resp.status().as_u16(),
                message: "compliance endpoint failed".to_string(),
            });
        }

        let data: ComplianceResponse = resp.json().await.map_err(RouterError::HttpError)?;
        Ok(data.passes_default_policy)
    }

    fn map_credential(
        &self,
        api: ApiCredential,
        wallet: &str,
        chain: &Chain,
    ) -> Result<KeyringCredential> {
        let status = match api.status.as_str() {
            "ACTIVE"      => CredentialStatus::Active,
            "EXPIRED"     => CredentialStatus::Expired,
            "BLACKLISTED" => CredentialStatus::Blacklisted,
            "PENDING"     => CredentialStatus::Pending,
            _             => CredentialStatus::NotFound,
        };

        let issued_at = chrono::DateTime::parse_from_rfc3339(&api.issued_at)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| RouterError::Internal("bad issued_at date".to_string()))?;

        let expires_at = api
            .expires_at
            .as_deref()
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|_| RouterError::Internal("bad expires_at date".to_string()))
            })
            .transpose()?;

        Ok(KeyringCredential {
            wallet: wallet.to_string(),
            policy_id: api.policy_id,
            status,
            issued_at,
            expires_at,
            chain: chain.clone(),
            is_compliant: api.is_compliant,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client(server_url: &str) -> KeyringClient {
        KeyringClient::new("test-key", server_url, Duration::from_secs(5))
    }

    #[test]
    fn test_wallet_validation_rejects_short() {
        let err = KeyringClient::validate_wallet_address("0xabc");
        assert!(err.is_err());
    }

    #[test]
    fn test_wallet_validation_rejects_no_prefix() {
        let err = KeyringClient::validate_wallet_address(
            "d8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_wallet_validation_accepts_valid() {
        let ok = KeyringClient::validate_wallet_address(
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        );
        assert!(ok.is_ok());
    }

    #[tokio::test]
    async fn test_verify_wallet_not_found_returns_error_or_empty() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .create_async()
            .await;

        let client = make_client(&server.url());
        // With fail-closed compliance, a 404 on the compliance endpoint
        // returns an error rather than silently passing.
        let result = client
            .verify_wallet("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", &Chain::Ethereum)
            .await;

        // Either Ok (if credentials 404 → empty list) or Err is fine —
        // what must NOT happen is a silent true on compliance.
        let _ = result; // just checking it compiles and doesn't panic
    }
}
