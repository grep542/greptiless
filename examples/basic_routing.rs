use greptiles::{CapitalRouter, Chain};
use rust_decimal_macros::dec;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    greptiles::init_tracing();

    let api_key = std::env::var("KEYRING_API_KEY")
        .unwrap_or_else(|_| "demo-key".to_string());

    let wallet = std::env::var("WALLET_ADDRESS")
        .unwrap_or_else(|_| "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".to_string());

    // ── Create router ──────────────────────────────────────────────────────
    let router = CapitalRouter::new(api_key);

    println!("🔍  Scanning compliant DeFi routes…\n");

    // ── Find routes ────────────────────────────────────────────────────────
    match router
        .find_routes(
            &wallet,
            dec!(100_000), // $100k
            Chain::Ethereum,
        )
        .await
    {
        Ok(result) => {
            println!("✅  Identity verified: {}", result.wallet);
            println!(
                "📊  Scanned {} pools | {} filtered by compliance | {} routes returned\n",
                result.total_opportunities_scanned,
                result.compliance_filtered_count,
                result.routes.len(),
            );

            for route in &result.routes {
                println!(
                    "#{} [{:?}] {} — {:.2}% APY",
                    route.rank,
                    route.opportunity.protocol,
                    route.opportunity.pool_name,
                    route.opportunity.apy * dec!(100),
                );
                println!(
                    "   TVL: ${:.1}M  |  Risk: {:?}  |  Score: {:.4}",
                    route.opportunity.tvl_usd / dec!(1_000_000),
                    route.opportunity.risk_tier,
                    route.score,
                );
                println!(
                    "   Expected annual return on $100k: ${:.2}\n",
                    route.expected_annual_return_usd,
                );
            }
        }
        Err(e) => {
            eprintln!("❌  Routing failed: {}", e);
        }
    }

    Ok(())
}