use greptiles::{
    CapitalRouter, Chain, KeyringClient, RouterConfig, RiskTier,
};
use rust_decimal_macros::dec;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    greptiles::init_tracing();

    let api_key = std::env::var("KEYRING_API_KEY")
        .unwrap_or_else(|_| "demo-key".to_string());
    let graph_key = std::env::var("GRAPH_API_KEY").ok();
    let wallet = std::env::var("WALLET_ADDRESS")
        .unwrap_or_else(|_| "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".to_string());

    // ── Step 1: Manual identity check (optional – router does this too) ──
    println!("🪪  Running identity pre-check…");
    let keyring = KeyringClient::new(
        &api_key,
        "https://api.keyring.network",
        Duration::from_secs(15),
    );

    match keyring.verify_wallet(&wallet, &Chain::Ethereum).await {
        Ok(identity) => {
            println!("   Wallet:   {}", identity.wallet);
            println!("   Default policy: {}", identity.passes_default_policy);
            println!("   Credentials:    {}", identity.credentials.len());
            for cred in &identity.credentials {
                println!(
                    "     Policy {:>3} — {:?}  (valid: {})",
                    cred.policy_id,
                    cred.status,
                    cred.is_valid()
                );
            }
        }
        Err(e) => {
            eprintln!("   Identity check failed: {}", e);
        }
    }

    println!();

    let config = RouterConfig::new(&api_key)
        .with_graph_api_key(graph_key.unwrap_or_default())
        .with_max_routes(3)
        .with_min_apy(dec!(0.02))           // ≥ 2% APY minimum
        .with_max_risk_tier(RiskTier::Low)  // Low-risk only for institutions
        .with_min_tvl(dec!(50_000_000))     // ≥ $50M TVL
        .require_keyring_gate(false);       // set true in prod for gated pools

    let router = CapitalRouter::with_config(config);

    let capital = dec!(5_000_000);
    println!("💰  Routing ${:.0} on Ethereum (institutional, low-risk)…\n", capital);

    match router.find_routes(&wallet, capital, Chain::Ethereum).await {
        Ok(result) => {
            println!(
                "📊  {} routes  |  {} scanned  |  {} filtered\n",
                result.routes.len(),
                result.total_opportunities_scanned,
                result.compliance_filtered_count,
            );

            for route in &result.routes {
                let opp = &route.opportunity;
                println!("──────────────────────────────────────────────────");
                println!(
                    "  #{} {} [{}]",
                    route.rank, opp.pool_name, opp.protocol
                );
                println!("  APY:             {:.3}%", opp.apy * dec!(100));
                println!("  TVL:             ${:.1}M", opp.tvl_usd / dec!(1_000_000));
                println!("  Risk:            {:?}", opp.risk_tier);
                println!("  Keyring gated:   {}", opp.has_keyring_gate);
                println!(
                    "  Expected return: ${:.2} / year",
                    route.expected_annual_return_usd
                );
                println!("  Score:           {:.4}", route.score);
                println!("  Rationale:       {}", route.rationale);

                if !route.compliance.rejection_reasons.is_empty() {
                    println!(
                        "  ⚠️  Compliance notes: {:?}",
                        route.compliance.rejection_reasons
                    );
                }
            }

            println!("\n✅  Routing complete at {}", result.computed_at);
        }
        Err(e) => {
            eprintln!("❌  Routing error: {}", e);
        }
    }

    Ok(())
}