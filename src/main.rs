use clap::{Parser, ValueEnum};
use greptiles::{CapitalRouter, Chain, RouterConfig, RiskTier};
use rust_decimal::Decimal;
use std::str::FromStr;

#[derive(Parser, Debug)]
#[command(
    name = "greptiles",
    about = "Compliance-aware DeFi capital router with predictive Markowitz allocation",
    version = "0.2.0"
)]
struct Args {
    /// Ethereum wallet address (0x...)
    #[arg(short, long)]
    wallet: String,

    /// Capital amount in USD (e.g. 100000)
    #[arg(short, long)]
    capital: String,

    #[arg(short = 'n', long, default_value = "ethereum")]
    chain: ChainArg,

    #[arg(short, long, default_value = "medium")]
    risk: RiskArg,

    #[arg(long, default_value = "5")]
    routes: usize,

    /// Minimum APY % (e.g. 1 = 1%)
    #[arg(long, default_value = "1")]
    min_apy: f64,

    #[arg(long, default_value = "1000000")]
    min_tvl: f64,

    #[arg(long, default_value = "false")]
    gated_only: bool,

    /// Markowitz risk aversion λ (0.5 = aggressive, 10.0 = conservative)
    #[arg(long, default_value = "2.0")]
    risk_aversion: f64,

    /// Output raw JSON instead of formatted table
    #[arg(long, default_value = "false")]
    json: bool,
}

#[derive(Debug, Clone, ValueEnum)]
enum ChainArg { Ethereum, Arbitrum, Optimism, Base, Avalanche, Polygon }

impl From<ChainArg> for Chain {
    fn from(c: ChainArg) -> Self {
        match c {
            ChainArg::Ethereum  => Chain::Ethereum,
            ChainArg::Arbitrum  => Chain::Arbitrum,
            ChainArg::Optimism  => Chain::Optimism,
            ChainArg::Base      => Chain::Base,
            ChainArg::Avalanche => Chain::Avalanche,
            ChainArg::Polygon   => Chain::Polygon,
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum RiskArg { Low, Medium, High }

impl From<RiskArg> for RiskTier {
    fn from(r: RiskArg) -> Self {
        match r {
            RiskArg::Low    => RiskTier::Low,
            RiskArg::Medium => RiskTier::Medium,
            RiskArg::High   => RiskTier::High,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    greptiles::init_tracing();
    let args = Args::parse();

    let api_key = std::env::var("KEYRING_API_KEY").unwrap_or_else(|_| {
        eprintln!("⚠️  KEYRING_API_KEY not set — using demo mode");
        "demo-key".to_string()
    });
    let graph_key = std::env::var("GRAPH_API_KEY").ok();

    // Accept capital as a string to avoid f64 precision issues
    let capital = Decimal::from_str(&args.capital)
        .map_err(|_| anyhow::anyhow!("--capital must be a valid decimal number e.g. 100000"))?;

    let chain: Chain    = args.chain.into();
    let risk: RiskTier  = args.risk.into();

    let mut config = RouterConfig::new(api_key)
        .with_max_routes(args.routes)
        .with_min_apy(Decimal::from_str(&format!("{:.10}", args.min_apy / 100.0))?)
        .with_min_tvl(Decimal::from_str(&args.min_tvl.to_string())?)
        .with_max_risk_tier(risk)
        .require_keyring_gate(args.gated_only)
        .with_risk_aversion(args.risk_aversion);

    if let Some(key) = graph_key {
        config = config.with_graph_api_key(key);
    }

    let router = CapitalRouter::with_config(config);

    if !args.json {
        println!("\n🔍  Greptiles v0.2 — Predictive Compliance-Aware Capital Router");
        println!("    Wallet:         {}", args.wallet);
        println!("    Capital:        ${}", args.capital);
        println!("    Chain:          {}", chain);
        println!("    Risk aversion:  λ={}\n", args.risk_aversion);
    }

    match router.find_routes(&args.wallet, capital, chain).await {
        Ok(result) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }

            println!(
                "✅  Identity verified  |  {} scanned  |  {} compliance-filtered  |  {} routes\n",
                result.total_opportunities_scanned,
                result.compliance_filtered_count,
                result.routes.len(),
            );

            if result.routes.is_empty() {
                println!("⚠️  No compliant routes found. Try lowering --min-apy or --min-tvl.");
                return Ok(());
            }

            let total_alloc: Decimal = result.routes.iter()
                .map(|r| r.recommended_allocation_usd)
                .sum();
            let total_return: Decimal = result.routes.iter()
                .map(|r| r.expected_annual_return_usd)
                .sum();

            for route in &result.routes {
                let opp = &route.opportunity;
                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!(
                    "  #{} {:?} — {}",
                    route.rank, opp.protocol, opp.pool_name
                );
                println!("  Allocation:       ${:.2}  ({:.1}%)",
                    route.recommended_allocation_usd,
                    Decimal::to_f64(&route.weight).unwrap_or(0.0) * 100.0,
                );
                println!("  Predicted APY:    {:.2}%  (spot: {:.2}%)",
                    Decimal::to_f64(&route.predicted_apy).unwrap_or(0.0) * 100.0,
                    Decimal::to_f64(&opp.apy).unwrap_or(0.0) * 100.0,
                );
                println!("  Forecast conf.:   {:.0}%",
                    Decimal::to_f64(&route.forecast_confidence).unwrap_or(0.0) * 100.0,
                );
                println!("  TVL:              ${:.1}M",
                    opp.tvl_usd / Decimal::from(1_000_000)
                );
                println!("  Risk tier:        {}", opp.risk_tier);
                println!("  Keyring gated:    {}", opp.has_keyring_gate);
                println!("  Expected return:  ${:.2} / year", route.expected_annual_return_usd);
                println!("  💡 {}", route.rationale);
                println!();
            }

            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("  Total allocated:  ${:.2}", total_alloc);
            println!("  Total est. return: ${:.2} / year", total_return);
            println!("  Risk aversion λ:  {}", result.risk_aversion);
            println!("  Computed at:      {}", result.computed_at);
        }
        Err(e) => {
            if args.json {
                println!("{{\"error\": \"{}\"}}", e);
            } else {
                eprintln!("\n❌  Error: {}", e);
                eprintln!("    Make sure your wallet has a valid Keyring credential.");
                eprintln!("    See: https://keyring.network\n");
            }
            std::process::exit(1);
        }
    }

    Ok(())
}
