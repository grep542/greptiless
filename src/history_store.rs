use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::error::{Result, RouterError};
use crate::models::ApyDataPoint;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Abstraction over APY history backends.
/// Implement this trait to swap in TimescaleDB, Redis, etc.
#[async_trait::async_trait]
pub trait ApyHistoryStore: Send + Sync {
    /// Record a new observation for a pool.
    async fn append(&self, point: ApyDataPoint) -> Result<()>;

    /// Fetch the last `days` days of observations for a pool.
    async fn fetch_window(&self, pool_id: &str, days: u32) -> Result<Vec<ApyDataPoint>>;

    /// Fetch latest observations for multiple pools in one call.
    async fn fetch_batch(
        &self,
        pool_ids: &[&str],
        days: u32,
    ) -> Result<HashMap<String, Vec<f64>>>;
}

// ── In-memory implementation ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct StorageInner {
    /// pool_id → chronological APY observations (newest last).
    series: HashMap<String, Vec<ApyDataPoint>>,
}

impl StorageInner {
    fn new() -> Self {
        Self { series: HashMap::new() }
    }
}

/// Thread-safe in-memory store with optional JSON file persistence.
#[derive(Clone)]
pub struct InMemoryApyStore {
    inner: Arc<RwLock<StorageInner>>,
    /// Maximum number of data points to retain per pool.
    /// At one observation per 15 minutes: 2880 ≈ 30 days.
    max_points_per_pool: usize,
}

impl InMemoryApyStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StorageInner::new())),
            max_points_per_pool: 2880,
        }
    }

    pub fn with_max_points(mut self, n: usize) -> Self {
        self.max_points_per_pool = n;
        self
    }

    /// Persist state to a JSON file (call periodically or on shutdown).
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let inner = self.inner.read().map_err(|e| {
            RouterError::Internal(format!("lock poisoned: {}", e))
        })?;
        let json = serde_json::to_string(&*inner)
            .map_err(RouterError::DeserializationError)?;
        std::fs::write(path, json)
            .map_err(|e| RouterError::Internal(format!("file write failed: {}", e)))?;
        Ok(())
    }

    /// Restore state from a JSON file written by `save_to_file`.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| RouterError::Internal(format!("file read failed: {}", e)))?;
        let inner: StorageInner = serde_json::from_str(&json)
            .map_err(RouterError::DeserializationError)?;
        info!(
            "Loaded APY history for {} pools from file",
            inner.series.len()
        );
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
            max_points_per_pool: 2880,
        })
    }

    /// Number of pools currently tracked.
    pub fn pool_count(&self) -> usize {
        self.inner.read().map(|g| g.series.len()).unwrap_or(0)
    }
}

impl Default for InMemoryApyStore {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait]
impl ApyHistoryStore for InMemoryApyStore {
    async fn append(&self, point: ApyDataPoint) -> Result<()> {
        let mut inner = self.inner.write().map_err(|e| {
            RouterError::Internal(format!("lock poisoned: {}", e))
        })?;

        let series = inner.series
            .entry(point.pool_id.clone())
            .or_default();

        series.push(point);

        // Trim to rolling window
        if series.len() > self.max_points_per_pool {
            let excess = series.len() - self.max_points_per_pool;
            series.drain(0..excess);
        }

        debug!(
            "Appended APY point for pool {}; series len={}",
            series.last().map(|p| p.pool_id.as_str()).unwrap_or("?"),
            series.len()
        );

        Ok(())
    }

    async fn fetch_window(&self, pool_id: &str, days: u32) -> Result<Vec<ApyDataPoint>> {
        let inner = self.inner.read().map_err(|e| {
            RouterError::Internal(format!("lock poisoned: {}", e))
        })?;

        let cutoff: DateTime<Utc> = Utc::now() - Duration::days(days as i64);

        Ok(inner
            .series
            .get(pool_id)
            .map(|series| {
                series
                    .iter()
                    .filter(|p| p.timestamp >= cutoff)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn fetch_batch(
        &self,
        pool_ids: &[&str],
        days: u32,
    ) -> Result<HashMap<String, Vec<f64>>> {
        let inner = self.inner.read().map_err(|e| {
            RouterError::Internal(format!("lock poisoned: {}", e))
        })?;

        let cutoff: DateTime<Utc> = Utc::now() - Duration::days(days as i64);
        let mut result = HashMap::new();

        for &id in pool_ids {
            let apys: Vec<f64> = inner
                .series
                .get(id)
                .map(|series| {
                    series
                        .iter()
                        .filter(|p| p.timestamp >= cutoff)
                        .map(|p| p.apy)
                        .collect()
                })
                .unwrap_or_default();

            if !apys.is_empty() {
                result.insert(id.to_string(), apys);
            }
        }

        Ok(result)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_point(pool_id: &str, apy: f64) -> ApyDataPoint {
        ApyDataPoint {
            pool_id: pool_id.to_string(),
            apy,
            timestamp: Utc::now(),
            utilization_rate: None,
        }
    }

    #[tokio::test]
    async fn test_append_and_fetch() {
        let store = InMemoryApyStore::new();
        store.append(make_point("pool-a", 0.05)).await.unwrap();
        store.append(make_point("pool-a", 0.051)).await.unwrap();

        let points = store.fetch_window("pool-a", 7).await.unwrap();
        assert_eq!(points.len(), 2);
    }

    #[tokio::test]
    async fn test_fetch_unknown_pool_returns_empty() {
        let store = InMemoryApyStore::new();
        let points = store.fetch_window("no-such-pool", 7).await.unwrap();
        assert!(points.is_empty());
    }

    #[tokio::test]
    async fn test_batch_fetch() {
        let store = InMemoryApyStore::new();
        store.append(make_point("pool-a", 0.05)).await.unwrap();
        store.append(make_point("pool-b", 0.04)).await.unwrap();

        let batch = store.fetch_batch(&["pool-a", "pool-b", "pool-c"], 7).await.unwrap();
        assert!(batch.contains_key("pool-a"));
        assert!(batch.contains_key("pool-b"));
        assert!(!batch.contains_key("pool-c")); // no data → excluded
    }

    #[tokio::test]
    async fn test_rolling_window_trims_old_points() {
        let store = InMemoryApyStore::new().with_max_points(3);
        for i in 0..5u32 {
            store.append(make_point("pool-x", 0.05 + i as f64 * 0.001)).await.unwrap();
        }
        // Internal series should be trimmed to 3
        assert_eq!(store.pool_count(), 1);
        let pts = store.fetch_window("pool-x", 365).await.unwrap();
        assert_eq!(pts.len(), 3);
    }
}
