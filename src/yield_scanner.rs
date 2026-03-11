use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use crate::error::{Result, RouterError};
use crate::models::{Chain, Protocol, RiskTier, YieldOpportunity};

// Updated to Aave's decentralised subgraph endpoints (hosted service deprecated).
const AAVE_V3_SUBGRAPH_MAINNET: &str =
    "https://gateway.thegraph.com/api/subgraphs/id/Cd2gEDVeqnjBn1hSeqFMitw8Q1iiyV9FYUZkLNRcL87g";
const AAVE_V3_SUBGRAPH_ARBITRUM: &str =
    "https://gateway.thegraph.com/api/subgraphs/id/DLuE98kEb5pQNXAcKFQGQgfSQ57Xdou4jnVbAEqMfy3B";
const COMPOUND_API_BASE: &str = "https://api.compound.finance/api/v2";
const LIDO_APR_API: &str = "https://eth-api.lido.fi/v1/protocol/steth/apr/sma";
/// DeFiLlama protocol endpoint for live Lido TVL.
const DEFILLAMA_LIDO_TVL: &str = "https://api.llama.fi/protocol/lido";

#[derive(Debug, Deserialize)]
struct GraphResponse<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct AaveMarketsData {
    reserves: Vec<AaveReserve>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AaveReserve {
    id: String,
    name: String,
    symbol: String,
    underlying_asset: String,
    liquidity_rate: String,  
    total_liquidity: String, 
    available_liquidity: String,
    total_value_locked_usd: Option<String>,
    decimals: u8,
}

impl AaveReserve {
    fn apy_from_ray(&self) -> Decimal {
        let ray: u128 = self.liquidity_rate.parse().unwrap_or(0);
        let apy = ray as f64 / 1e27;
        Decimal::from_f64(apy).unwrap_or_default()
    }

    fn tvl_usd(&self) -> Decimal {
        self.total_value_locked_usd
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default()
    }

    fn available_liquidity_usd(&self) -> Decimal {
        let raw: u128 = self.available_liquidity.parse().unwrap_or(0);
        let divisor = 10u128.pow(self.decimals as u32) as f64;
        Decimal::from_f64(raw as f64 / divisor).unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct CompoundMarketsResponse {
    markets: Vec<CompoundMarket>,
}

#[derive(Debug, Deserialize)]
struct CompoundMarket {
    token_address: String,
    symbol: String,
    supply_rate: CompoundRate,
    total_supply_value_in_eth: Option<CompoundValue>,
    cash: Option<CompoundValue>,
    #[serde(rename = "underlyingAddress")]
    underlying_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompoundRate {
    value: String,
}

#[derive(Debug, Deserialize)]
struct CompoundValue {
    value: String,
}

#[derive(Debug, Deserialize)]
struct LidoAprResponse {
    data: LidoAprData,
}

#[derive(Debug, Deserialize)]
struct LidoAprData {
    smaApr: f64,
}

#[derive(Debug, Deserialize)]
struct DefiLlamaTvlResponse {
    /// Current total value locked in USD.
    tvl: Option<f64>,
}

#[derive(Clone)]
pub struct YieldScanner {
    http: Client,
    graph_api_key: Option<String>,
}

impl YieldScanner {
    pub fn new(timeout: Duration, graph_api_key: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent("greptiles/0.1.0")
            .build()
            .expect("failed to build HTTP client");

        Self { http, graph_api_key }
    }

    #[instrument(skip(self), fields(chain = %chain))]
    pub async fn fetch_opportunities(&self, chain: &Chain) -> Result<Vec<YieldOpportunity>> {
        info!("Scanning yield opportunities on {}", chain);

        let mut opportunities = Vec::new();

        let (aave_res, compound_res, lido_res) = tokio::join!(
            self.fetch_aave(chain),
            self.fetch_compound(chain),
            self.fetch_lido(chain),
        );

        match aave_res {
            Ok(mut ops) => {
                debug!("Fetched {} Aave opportunities", ops.len());
                opportunities.append(&mut ops);
            }
            Err(e) => warn!("Aave fetch failed on {}: {}", chain, e),
        }

        match compound_res {
            Ok(mut ops) => {
                debug!("Fetched {} Compound opportunities", ops.len());
                opportunities.append(&mut ops);
            }
            Err(e) => warn!("Compound fetch failed on {}: {}", chain, e),
        }

        match lido_res {
            Ok(mut ops) => {
                debug!("Fetched {} Lido opportunities", ops.len());
                opportunities.append(&mut ops);
            }
            Err(e) => warn!("Lido fetch failed on {}: {}", chain, e),
        }

        info!(
            "Total opportunities fetched on {}: {}",
            chain,
            opportunities.len()
        );

        Ok(opportunities)
    }


    async fn fetch_aave(&self, chain: &Chain) -> Result<Vec<YieldOpportunity>> {
        let subgraph_url = match chain {
            Chain::Ethereum => AAVE_V3_SUBGRAPH_MAINNET,
            Chain::Arbitrum => AAVE_V3_SUBGRAPH_ARBITRUM,
            _ => {
                debug!("Aave v3 not supported on {}", chain);
                return Ok(vec![]);
            }
        };

        let query = r#"
        {
          reserves(
            where: { isActive: true, isFrozen: false }
            first: 50
            orderBy: totalValueLockedUSD
            orderDirection: desc
          ) {
            id
            name
            symbol
            underlyingAsset
            liquidityRate
            totalLiquidity
            availableLiquidity
            totalValueLockedUSD
            decimals
          }
        }
        "#;

        let body = serde_json::json!({ "query": query });

        let resp = self
            .http
            .post(subgraph_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| RouterError::ProtocolFetchFailed {
                protocol: "Aave".into(),
                reason: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(RouterError::ProtocolFetchFailed {
                protocol: "Aave".into(),
                reason: format!("HTTP {}", resp.status()),
            });
        }

        let data: GraphResponse<AaveMarketsData> = resp.json().await.map_err(|e| {
            RouterError::ProtocolFetchFailed {
                protocol: "Aave".into(),
                reason: e.to_string(),
            }
        })?;

        let now = Utc::now();
        let opps = data
            .data
            .reserves
            .into_iter()
            .filter(|r| r.apy_from_ray() > Decimal::ZERO)
            .map(|r| {
                let apy = r.apy_from_ray();
                YieldOpportunity {
                    id: format!("aave-v3-{}-{}", chain, r.symbol.to_lowercase()),
                    protocol: Protocol::Aave,
                    chain: chain.clone(),
                    pool_name: format!("Aave v3 {} Supply Market", r.name),
                    pool_address: r.id.clone(),
                    token_symbol: r.symbol.clone(),
                    token_address: r.underlying_asset.clone(),
                    apy,
                    tvl_usd: r.tvl_usd(),
                    available_liquidity_usd: r.available_liquidity_usd(),
                    risk_tier: RiskTier::Low, // Aave v3 is battle-tested
                    has_keyring_gate: false,  // enriched by compliance layer
                    required_policy_id: None,
                    fetched_at: now,
                }
            })
            .collect();

        Ok(opps)
    }


    async fn fetch_compound(&self, chain: &Chain) -> Result<Vec<YieldOpportunity>> {
        // Compound v3 is primarily on Ethereum mainnet
        if chain != &Chain::Ethereum {
            return Ok(vec![]);
        }

        let url = format!("{}/markets?network=mainnet&page_size=20", COMPOUND_API_BASE);

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| RouterError::ProtocolFetchFailed {
                protocol: "Compound".into(),
                reason: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(RouterError::ProtocolFetchFailed {
                protocol: "Compound".into(),
                reason: format!("HTTP {}", resp.status()),
            });
        }

        let data: CompoundMarketsResponse = resp.json().await.map_err(|e| {
            RouterError::ProtocolFetchFailed {
                protocol: "Compound".into(),
                reason: e.to_string(),
            }
        })?;

        let now = Utc::now();
        let opps = data
            .markets
            .into_iter()
            .filter_map(|m| {
                let apy: Decimal = m.supply_rate.value.parse().ok()?;
                if apy == Decimal::ZERO {
                    return None;
                }
                let tvl = m
                    .total_supply_value_in_eth
                    .as_ref()
                    .and_then(|v| v.value.parse().ok())
                    .unwrap_or_default();
                let liquidity = m
                    .cash
                    .as_ref()
                    .and_then(|v| v.value.parse().ok())
                    .unwrap_or_default();

                Some(YieldOpportunity {
                    id: format!("compound-v3-{}-{}", chain, m.symbol.to_lowercase()),
                    protocol: Protocol::Compound,
                    chain: chain.clone(),
                    pool_name: format!("Compound v3 {} Supply", m.symbol),
                    pool_address: m.token_address.clone(),
                    token_symbol: m.symbol.clone(),
                    token_address: m
                        .underlying_address
                        .clone()
                        .unwrap_or(m.token_address.clone()),
                    apy,
                    tvl_usd: tvl,
                    available_liquidity_usd: liquidity,
                    risk_tier: RiskTier::Low,
                    has_keyring_gate: false,
                    required_policy_id: None,
                    fetched_at: now,
                })
            })
            .collect();

        Ok(opps)
    }


    async fn fetch_lido(&self, chain: &Chain) -> Result<Vec<YieldOpportunity>> {
        // Lido stETH only on Ethereum
        if chain != &Chain::Ethereum {
            return Ok(vec![]);
        }

        // Fetch APY and TVL concurrently
        let (apr_resp, tvl_resp) = tokio::join!(
            self.http.get(LIDO_APR_API).send(),
            self.http.get(DEFILLAMA_LIDO_TVL).send(),
        );

        let apr_resp = apr_resp.map_err(|e| RouterError::ProtocolFetchFailed {
            protocol: "Lido".into(),
            reason: e.to_string(),
        })?;

        if !apr_resp.status().is_success() {
            return Err(RouterError::ProtocolFetchFailed {
                protocol: "Lido".into(),
                reason: format!("APR endpoint HTTP {}", apr_resp.status()),
            });
        }

        let apr_data: LidoAprResponse = apr_resp.json().await.map_err(|e| {
            RouterError::ProtocolFetchFailed {
                protocol: "Lido".into(),
                reason: e.to_string(),
            }
        })?;

        let apy = Decimal::from_f64(apr_data.data.smaApr / 100.0).unwrap_or_default();

        // Fetch live TVL from DeFiLlama; fall back gracefully if it fails.
        let tvl_usd = match tvl_resp {
            Ok(resp) if resp.status().is_success() => {
                resp.json::<DefiLlamaTvlResponse>()
                    .await
                    .ok()
                    .and_then(|d| d.tvl)
                    .and_then(|v| Decimal::from_f64(v))
                    .unwrap_or_else(|| {
                        warn!("DeFiLlama TVL parse failed; using 0 for Lido TVL");
                        Decimal::ZERO
                    })
            }
            other => {
                warn!("DeFiLlama TVL fetch failed ({:?}); using 0 for Lido TVL", other.err());
                Decimal::ZERO
            }
        };

        // Available liquidity: Lido is liquid (ETH withdrawals enabled post-Shapella).
        // Use 10% of TVL as a conservative liquidity estimate.
        let available_liquidity_usd = tvl_usd / Decimal::from(10);

        Ok(vec![YieldOpportunity {
            id: "lido-steth-ethereum".to_string(),
            protocol: Protocol::Lido,
            chain: Chain::Ethereum,
            pool_name: "Lido stETH Staking".to_string(),
            pool_address: "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84".to_string(),
            token_symbol: "ETH".to_string(),
            token_address: "0x0000000000000000000000000000000000000000".to_string(),
            apy,
            tvl_usd,
            available_liquidity_usd,
            risk_tier: RiskTier::Low,
            has_keyring_gate: false,
            required_policy_id: None,
            fetched_at: Utc::now(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_ray_to_apy() {
        let reserve = AaveReserve {
            id: "0x1".to_string(),
            name: "USD Coin".to_string(),
            symbol: "USDC".to_string(),
            underlying_asset: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            // 5% APY in RAY = 0.05 * 1e27
            liquidity_rate: "50000000000000000000000000".to_string(),
            total_liquidity: "1000000000000".to_string(),
            available_liquidity: "500000000000".to_string(),
            total_value_locked_usd: Some("1000000".to_string()),
            decimals: 6,
        };
        let apy = reserve.apy_from_ray();
        assert!(apy > Decimal::ZERO);
        assert!(apy < dec!(1)); // sanity: < 100%
    }
}