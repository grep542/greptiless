use rust_decimal::Decimal;
use tracing::{debug, info, instrument};

use crate::error::{Result, RouterError};
use crate::models::{
    Chain, ComplianceCheckResult, CredentialStatus, IdentityCheckResult, RiskTier,
    YieldOpportunity,
};

#[derive(Debug, Clone)]
pub struct ComplianceConfig {
    pub max_risk_tier: RiskTier,
    pub min_tvl_usd: Decimal,
    pub min_apy: Decimal,
    pub require_keyring_gate: bool,
    pub required_policy_id: Option<u32>,
}

pub struct ComplianceFilter;

impl ComplianceFilter {
    #[instrument(skip_all, fields(
        wallet = %identity.wallet,
        total = opportunities.len(),
    ))]
    pub fn filter(
        identity: &IdentityCheckResult,
        opportunities: Vec<YieldOpportunity>,
        config: &ComplianceConfig,
    ) -> Result<(
        Vec<(YieldOpportunity, ComplianceCheckResult)>,
        Vec<(YieldOpportunity, ComplianceCheckResult)>,
    )> {

        if !identity.passes_default_policy && identity.credentials.is_empty() {
            return Err(RouterError::IdentityCheckFailed {
                wallet: identity.wallet.clone(),
                reason: "wallet has no Keyring credentials".to_string(),
            });
        }

        if identity
            .credentials
            .iter()
            .any(|c| c.status == CredentialStatus::Blacklisted)
        {
            return Err(RouterError::IdentityCheckFailed {
                wallet: identity.wallet.clone(),
                reason: "wallet is blacklisted on one or more policies".to_string(),
            });
        }

        let total = opportunities.len();
        let mut compliant = Vec::new();
        let mut rejected = Vec::new();

        for opp in opportunities {
            let check = Self::check_single(&opp, identity, config);
            if check.is_compliant {
                compliant.push((opp, check));
            } else {
                rejected.push((opp, check));
            }
        }

        info!(
            "Compliance filter: {}/{} opportunities passed",
            compliant.len(),
            total
        );

        Ok((compliant, rejected))
    }

    fn check_single(
        opp: &YieldOpportunity,
        identity: &IdentityCheckResult,
        config: &ComplianceConfig,
    ) -> ComplianceCheckResult {
        let mut reasons: Vec<String> = Vec::new();
        let mut evaluated_policies: Vec<u32> = Vec::new();

        if opp.risk_tier > config.max_risk_tier {
            reasons.push(format!(
                "risk tier {:?} exceeds maximum {:?}",
                opp.risk_tier, config.max_risk_tier
            ));
        }

        if opp.tvl_usd < config.min_tvl_usd {
            reasons.push(format!(
                "TVL ${} below minimum ${}",
                opp.tvl_usd, config.min_tvl_usd
            ));
        }

        if opp.apy < config.min_apy {
            reasons.push(format!(
                "APY {:.2}% below minimum {:.2}%",
                opp.apy * Decimal::ONE_HUNDRED,
                config.min_apy * Decimal::ONE_HUNDRED,
            ));
        }

        if config.require_keyring_gate && !opp.has_keyring_gate {
            reasons.push("pool does not have a Keyring on-chain gate".to_string());
        }

        if let Some(required_policy) = opp.required_policy_id {
            evaluated_policies.push(required_policy);

            let credential = identity
                .credentials
                .iter()
                .find(|c| c.policy_id == required_policy);

            match credential {
                None => {
                    reasons.push(format!(
                        "wallet has no credential for required policy {}",
                        required_policy
                    ));
                }
                Some(c) if !c.is_valid() => {
                    reasons.push(format!(
                        "credential for policy {} is {:?} (not valid)",
                        required_policy, c.status
                    ));
                }
                Some(_) => {
                    debug!(
                        "Wallet passes policy {} for pool {}",
                        required_policy, opp.id
                    );
                }
            }
        }

        if let Some(required_policy) = config.required_policy_id {
            evaluated_policies.push(required_policy);

            let passes = identity
                .credentials
                .iter()
                .any(|c| c.policy_id == required_policy && c.is_valid());

            if !passes {
                reasons.push(format!(
                    "wallet does not pass configured required policy {}",
                    required_policy
                ));
            }
        }

        let is_compliant = reasons.is_empty();

        ComplianceCheckResult {
            opportunity_id: opp.id.clone(),
            is_compliant,
            rejection_reasons: reasons,
            evaluated_policies,
        }
    }


    pub fn enrich_with_keyring_gates(
        opportunities: Vec<YieldOpportunity>,
        gated_pools: &[(String, u32)], // (pool_address, policy_id)
    ) -> Vec<YieldOpportunity> {
        opportunities
            .into_iter()
            .map(|mut opp| {
                if let Some((_, policy_id)) = gated_pools
                    .iter()
                    .find(|(addr, _)| addr.eq_ignore_ascii_case(&opp.pool_address))
                {
                    opp.has_keyring_gate = true;
                    opp.required_policy_id = Some(*policy_id);
                }
                opp
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_identity(wallet: &str) -> IdentityCheckResult {
        IdentityCheckResult {
            wallet: wallet.to_string(),
            chain: Chain::Ethereum,
            credentials: vec![KeyringCredential {
                wallet: wallet.to_string(),
                policy_id: 0,
                status: CredentialStatus::Active,
                issued_at: Utc::now(),
                expires_at: None,
                chain: Chain::Ethereum,
                is_compliant: true,
            }],
            passes_default_policy: true,
            checked_at: Utc::now(),
        }
    }

    fn make_opportunity(apy: Decimal, tvl: Decimal, risk: RiskTier) -> YieldOpportunity {
        YieldOpportunity {
            id: "test-opp".to_string(),
            protocol: Protocol::Aave,
            chain: Chain::Ethereum,
            pool_name: "Test Pool".to_string(),
            pool_address: "0x1234".to_string(),
            token_symbol: "USDC".to_string(),
            token_address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
            apy,
            tvl_usd: tvl,
            available_liquidity_usd: dec!(1_000_000),
            risk_tier: risk,
            has_keyring_gate: false,
            required_policy_id: None,
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn test_compliant_opportunity_passes() {
        let identity = make_identity("0xabc");
        let opp = make_opportunity(dec!(0.05), dec!(10_000_000), RiskTier::Low);
        let config = ComplianceConfig {
            max_risk_tier: RiskTier::Medium,
            min_tvl_usd: dec!(1_000_000),
            min_apy: dec!(0.01),
            require_keyring_gate: false,
            required_policy_id: None,
        };

        let (compliant, rejected) =
            ComplianceFilter::filter(&identity, vec![opp], &config).unwrap();

        assert_eq!(compliant.len(), 1);
        assert_eq!(rejected.len(), 0);
    }

    #[test]
    fn test_high_risk_is_rejected() {
        let identity = make_identity("0xabc");
        let opp = make_opportunity(dec!(0.20), dec!(10_000_000), RiskTier::High);
        let config = ComplianceConfig {
            max_risk_tier: RiskTier::Medium,
            min_tvl_usd: dec!(1_000_000),
            min_apy: dec!(0.01),
            require_keyring_gate: false,
            required_policy_id: None,
        };

        let (compliant, rejected) =
            ComplianceFilter::filter(&identity, vec![opp], &config).unwrap();

        assert_eq!(compliant.len(), 0);
        assert_eq!(rejected.len(), 1);
        assert!(!rejected[0].1.rejection_reasons.is_empty());
    }

    #[test]
    fn test_low_tvl_is_rejected() {
        let identity = make_identity("0xabc");
        let opp = make_opportunity(dec!(0.05), dec!(100_000), RiskTier::Low);
        let config = ComplianceConfig {
            max_risk_tier: RiskTier::High,
            min_tvl_usd: dec!(1_000_000),
            min_apy: dec!(0.01),
            require_keyring_gate: false,
            required_policy_id: None,
        };

        let (compliant, rejected) =
            ComplianceFilter::filter(&identity, vec![opp], &config).unwrap();

        assert_eq!(compliant.len(), 0);
        assert!(rejected[0].1.rejection_reasons[0].contains("TVL"));
    }

    #[test]
    fn test_missing_policy_credential_rejected() {
        let identity = make_identity("0xabc"); // only has policy 0
        let mut opp = make_opportunity(dec!(0.05), dec!(10_000_000), RiskTier::Low);
        opp.has_keyring_gate = true;
        opp.required_policy_id = Some(42); // needs policy 42

        let config = ComplianceConfig {
            max_risk_tier: RiskTier::High,
            min_tvl_usd: dec!(0),
            min_apy: dec!(0),
            require_keyring_gate: false,
            required_policy_id: None,
        };

        let (compliant, rejected) =
            ComplianceFilter::filter(&identity, vec![opp], &config).unwrap();

        assert_eq!(compliant.len(), 0);
        assert!(rejected[0].1.rejection_reasons[0].contains("policy 42"));
    }
}