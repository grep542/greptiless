# greptiles

A **compliance-aware DeFi capital router** built in Rust. Uses [Keyring Network](https://keyring.network) identity verification to ensure institutional capital is routed only to verified, compliant DeFi opportunities.

## What it does

| Step | Module | Description |
|------|--------|-------------|
| 1 | `keyring_client` | Verify wallet credentials via Keyring REST API + on-chain contract |
| 2 | `yield_scanner` | Fetch live APY/TVL data from Aave v3, Compound v3, Lido |
| 3 | `compliance` | Filter pools by risk tier, TVL floor, APY floor, Keyring policy |
| 4 | `router` | Score + rank compliant opportunities; return best routes |

## Quick start

```rust
use greptiles::{CapitalRouter, Chain};
use rust_decimal_macros::dec;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let router = CapitalRouter::new("YOUR_KEYRING_API_KEY");

    let routes = router
        .find_routes(
            "0xYourWalletAddress",
            dec!(100_000),
            Chain::Ethereum,
        )
        .await?;

    for route in &routes.routes {
        println!("#{} {} — {:.2}% APY  score={:.4}",
            route.rank,
            route.opportunity.pool_name,
            route.opportunity.apy * dec!(100),
            route.score,
        );
    }
    Ok(())
}
```

## Architecture

```
src/
├── lib.rs              # Public SDK surface, re-exports
├── models.rs           # Chain, YieldOpportunity, CapitalRoute, RouterConfig …
├── error.rs            # RouterError enum
├── keyring_client.rs   # Keyring REST API + on-chain checks (ethers-rs)
├── yield_scanner.rs    # Aave v3 (The Graph) + Compound + Lido
├── compliance.rs       # Stateless compliance filter
└── router.rs           # CapitalRouter – orchestrates steps 1-4
```

## Scoring model

```
score = 0.60 × normalized_apy
      + 0.30 × normalized_tvl
      − 0.10 × risk_penalty
```

## Keyring Core Contract addresses

| Chain | Address |
|-------|---------|
| Ethereum | `0xb0B5E2176E10B12d70e60E3a68738298A7DFe666` |
| Arbitrum | `0xf26b0F10691ED160734a3A5caf8cA1FCb57eFc9d` |
| Base / Optimism | `0xf26b0f10691ed160734a3a5caf8ca1fcb57efc9d` |

