use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tracing::{info, instrument, warn};

use crate::compliance::{ComplianceConfig, ComplianceFilter};
use crate::error::{Result, RouterError};
use crate::forecaster::{AR1Model, CovarianceMatrix};
use crate::history_store::{ApyHistoryStore, InMemoryApyStore};
use crate::keyring_client::KeyringClient;
use crate::models::{
    ApyDataPoint, CapitalRoute, Chain, ComplianceCheckResult, RouterConfig, RoutingResult,
    YieldOpportunity,
};
use crate::optimizer::AllocationOptimizer;
use crate::yield_scanner::YieldScanner;

pub struct CapitalRouter {
    config: RouterConfig,
    keyring: KeyringClient,
    scanner: YieldScanner,
    history: Arc<dyn ApyHistoryStore>,
}

impl CapitalRouter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config(RouterConfig::new(api_key))
    }

    pub fn with_config(config: RouterConfig) -> Self {
        let timeout = Duration::from_secs(config.request_timeout_secs);
        let keyring = KeyringClient::new(
            config.keyring_api_key.as_str(),
            config.keyring_api_base_url.clone(),
            timeout,
        );
        let scanner = YieldScanner::new(timeout, config.graph_api_key.clone());
        let history = Arc::new(InMemoryApyStore::new());

        Self { config, keyring, scanner, history }
    }

    pub fn with_history_store(mut self, store: Arc<dyn ApyHistoryStore>) -> Self {
        self.history = store;
        self
    }

    #[instrument(skip(self), fields(
        wallet  = %wallet_address,
        capital = %capital_amount_usd,
        chain   = %chain,
    ))]
    pub async fn find_routes(
        &self,
        wallet_address: &str,
        capital_amount_usd: Decimal,
        chain: Chain,
    ) -> Result<RoutingResult> {
        info!(
            "find_routes: wallet={} capital=${} chain={}",
            wallet_address, capital_amount_usd, chain
        );

        let identity = self.keyring.verify_wallet(wallet_address, &chain).await?;

        if !identity.passes_default_policy {
            return Err(RouterError::IdentityCheckFailed {
                wallet: wallet_address.to_string(),
                reason: "wallet does not pass Keyring default policy".to_string(),
            });
        }

        info!(
            "Identity OK for {}: {} credentials",
            wallet_address, identity.credentials.len()
        );

        let raw_opportunities = self.scanner.fetch_opportunities(&chain).await?;
        let total_scanned = raw_opportunities.len();

        if raw_opportunities.is_empty() {
            return Err(RouterError::NoOpportunitiesFound {
                chain: chain.to_string(),
                min_apy: format!("{:.2}%", self.config.min_apy * Decimal::ONE_HUNDRED),
            });
        }

        for opp in &raw_opportunities {
            let apy_f = match Decimal::to_f64(&opp.apy) {
                Some(v) if v > 0.0 => v,
                _ => continue,
            };
            let point = ApyDataPoint {
                pool_id: opp.id.clone(),
                apy: apy_f,
                timestamp: Utc::now(),
                utilization_rate: None,
            };
            if let Err(e) = self.history.append(point).await {
                warn!("Failed to persist APY for {}: {}", opp.id, e);
            }
        }

        let protocols_with_data: std::collections::HashSet<_> = raw_opportunities
            .iter()
            .map(|o| &o.protocol)
            .collect();
        if protocols_with_data.len() < 2 {
            warn!(
                "Only {} protocol(s) returned data on {}; data may be incomplete",
                protocols_with_data.len(),
                chain
            );
        }

        let compliance_config = ComplianceConfig {
            max_risk_tier: self.config.max_risk_tier.clone(),
            min_tvl_usd: self.config.min_tvl_usd,
            min_apy: self.config.min_apy,
            require_keyring_gate: self.config.require_keyring_gate,
            required_policy_id: None,
        };

        let (compliant, rejected) =
            ComplianceFilter::filter(&identity, raw_opportunities, &compliance_config)?;

        let compliance_filtered = rejected.len();

        if compliant.is_empty() {
            return Err(RouterError::NoCompliantOpportunities { total: total_scanned });
        }

        let mut routes = self
            .allocate_opportunities(compliant, capital_amount_usd)
            .await?;

        routes.truncate(self.config.max_routes);
        for (i, route) in routes.iter_mut().enumerate() {
            route.rank = (i + 1) as u32;
        }

        info!(
            "Routing complete: {} routes from {} scanned ({} filtered)",
            routes.len(), total_scanned, compliance_filtered
        );

        Ok(RoutingResult {
            wallet: wallet_address.to_string(),
            capital_amount_usd,
            chain,
            identity,
            routes,
            total_opportunities_scanned: total_scanned,
            compliance_filtered_count: compliance_filtered,
            risk_aversion: self.config.risk_aversion,
            computed_at: Utc::now(),
        })
    }


    async fn allocate_opportunities(
        &self,
        compliant: Vec<(YieldOpportunity, ComplianceCheckResult)>,
        capital: Decimal,
    ) -> Result<Vec<CapitalRoute>> {
        let pool_ids: Vec<&str> = compliant.iter().map(|(o, _)| o.id.as_str()).collect();
        let n = compliant.len();

        let histories = self
            .history
            .fetch_batch(&pool_ids, 30)
            .await
            .unwrap_or_default();

        let mut predicted_apys: Vec<f64> = Vec::with_capacity(n);
        let mut confidence_weights: Vec<f64> = Vec::with_capacity(n);

        for (opp, _) in &compliant {
            let current = Decimal::to_f64(&opp.apy).unwrap_or(0.0);

            match histories.get(&opp.id) {
                Some(hist) if hist.len() >= 10 => {
                    if let Some(model) = AR1Model::fit(hist) {
                        let (forecast, _ci) =
                            model.forecast(current, self.config.forecast_horizon_days);
                        let conf = model.confidence_weight(current, self.config.forecast_horizon_days);
                        predicted_apys.push(forecast);
                        confidence_weights.push(conf);
                    } else {
                        // Fit failed (e.g. zero variance) — use spot, low conf
                        predicted_apys.push(current);
                        confidence_weights.push(0.3);
                    }
                }
                _ => {
                    // No history yet — use spot APY, low confidence
                    predicted_apys.push(current);
                    confidence_weights.push(0.3);
                }
            }
        }

        let cov_matrix = if histories.len() >= 2 {
            let mut cov = CovarianceMatrix::compute(&histories);
            cov.regularize(0.05);
            // Re-order rows/cols to match compliant order
            reorder_cov_matrix(cov, &pool_ids)
        } else {
            // Not enough history: use diagonal (independence assumption)
            diagonal_cov(&predicted_apys)
        };

        let capital_f = Decimal::to_f64(&capital).unwrap_or(1.0).max(f64::EPSILON);
        let liquidity_caps: Vec<f64> = compliant
            .iter()
            .map(|(opp, _)| {
                let avail = Decimal::to_f64(&opp.available_liquidity_usd).unwrap_or(0.0);
                let cap = (avail * self.config.max_liquidity_fraction) / capital_f;
                cap.min(1.0).max(self.config.min_pool_weight)
            })
            .collect();

        let optimizer = AllocationOptimizer::new(
            self.config.risk_aversion,
            self.config.min_pool_weight,
            self.config.max_pool_weight,
        );
        let eligible = vec![true; n];
        let weights = optimizer.optimize(
            &predicted_apys,
            &cov_matrix,
            &eligible,
            &liquidity_caps,
            &confidence_weights,
        );

        let routes = compliant
            .into_iter()
            .zip(weights.iter())
            .zip(predicted_apys.iter())
            .zip(confidence_weights.iter())
            .filter(|(((_, _), &w), _)| w > 0.01) // drop dust allocations
            .map(|(((( opp, compliance), &w), &pred_apy), &conf)| {
                let weight_dec = Decimal::from_f64(w).unwrap_or_default();
                let pred_apy_dec = Decimal::from_f64(pred_apy).unwrap_or(opp.apy);
                let allocation = capital * weight_dec;
                let expected_return = allocation * pred_apy_dec;

                let rationale = format!(
                    "{:.1}% → {} | pred APY: {:.2}% ({}d) | spot: {:.2}% | conf: {:.0}% | risk: {}",
                    w * 100.0,
                    opp.pool_name,
                    pred_apy * 100.0,
                    self.config.forecast_horizon_days,
                    Decimal::to_f64(&opp.apy).unwrap_or(0.0) * 100.0,
                    conf * 100.0,
                    opp.risk_tier,
                );

                CapitalRoute {
                    rank: 0, // set by caller
                    recommended_allocation_usd: allocation,
                    expected_annual_return_usd: expected_return,
                    weight: weight_dec,
                    score: weight_dec, // weight IS the score in Markowitz sense
                    compliance,
                    rationale,
                    predicted_apy: pred_apy_dec,
                    forecast_confidence: Decimal::from_f64(conf).unwrap_or_default(),
                    opportunity: opp,
                }
            })
            .collect();

        Ok(routes)
    }
}


fn reorder_cov_matrix(cov: CovarianceMatrix, pool_ids: &[&str]) -> Vec<Vec<f64>> {
    let n = pool_ids.len();
    let mut matrix = vec![vec![1e-6_f64; n]; n];

    for (i, &id_i) in pool_ids.iter().enumerate() {
        let ri = cov.pool_ids.iter().position(|id| id == id_i);
        for (j, &id_j) in pool_ids.iter().enumerate() {
            let rj = cov.pool_ids.iter().position(|id| id == id_j);
            if let (Some(ri), Some(rj)) = (ri, rj) {
                matrix[i][j] = cov.matrix[ri][rj];
            } else if i == j {
                matrix[i][j] = 1e-4; // default diagonal variance
            }
        }
    }
    matrix
}

fn diagonal_cov(returns: &[f64]) -> Vec<Vec<f64>> {
    let n = returns.len();
    (0..n)
        .map(|i| {
            (0..n)
                .map(|j| if i == j { 1e-4 } else { 0.0 })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_default_config() {
        let router = CapitalRouter::new("test-key");
        assert_eq!(router.config.max_routes, 5);
        assert_eq!(router.config.risk_aversion, 2.0);
    }

    #[test]
    fn test_router_custom_config() {
        let config = RouterConfig::new("test-key")
            .with_max_routes(3)
            .with_risk_aversion(5.0)
            .with_max_pool_weight(0.30);
        let router = CapitalRouter::with_config(config);
        assert_eq!(router.config.max_routes, 3);
        assert_eq!(router.config.risk_aversion, 5.0);
        assert_eq!(router.config.max_pool_weight, 0.30);
    }

    #[test]
    fn test_diagonal_cov_shape() {
        let cov = diagonal_cov(&[0.05, 0.04, 0.06]);
        assert_eq!(cov.len(), 3);
        assert_eq!(cov[0].len(), 3);
        assert!((cov[0][0] - 1e-4).abs() < 1e-10);
        assert_eq!(cov[0][1], 0.0);
    }
}
