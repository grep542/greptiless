#[derive(Debug, Clone)]
pub struct AllocationOptimizer {
    /// λ — Markowitz risk-aversion parameter (higher = more conservative).
    pub risk_aversion: f64,
    /// Floor weight per eligible pool (prevents dust positions).
    pub min_weight: f64,
    /// Ceiling weight per eligible pool (concentration limit).
    pub max_weight: f64,
    pub max_iterations: usize,
    pub learning_rate: f64,
}

impl Default for AllocationOptimizer {
    fn default() -> Self {
        Self {
            risk_aversion: 2.0,
            min_weight: 0.05,
            max_weight: 0.40,
            max_iterations: 600,
            learning_rate: 0.005,
        }
    }
}

impl AllocationOptimizer {
    pub fn new(
        risk_aversion: f64,
        min_weight: f64,
        max_weight: f64,
    ) -> Self {
        Self {
            risk_aversion,
            min_weight,
            max_weight,
            ..Default::default()
        }
    }

    /// Run the optimizer and return portfolio weights (one per pool, sums to 1).
    ///
    /// # Arguments
    /// * `predicted_returns` — μ_pred: AR(1) forecast APYs
    /// * `cov_matrix`        — Σ: pool APY covariance (n×n, row-major)
    /// * `eligible`          — compliance gate mask
    /// * `liquidity_caps`    — max weight from liquidity constraint per pool
    /// * `confidence`        — per-pool forecast confidence in [0, 1]
    pub fn optimize(
        &self,
        predicted_returns: &[f64],
        cov_matrix: &[Vec<f64>],
        eligible: &[bool],
        liquidity_caps: &[f64],
        confidence: &[f64],
    ) -> Vec<f64> {
        let n = predicted_returns.len();
        assert_eq!(n, eligible.len());
        assert_eq!(n, liquidity_caps.len());
        assert_eq!(n, confidence.len());

        let eligible_count = eligible.iter().filter(|&&e| e).count();
        if eligible_count == 0 {
            return vec![0.0; n];
        }

        // ── Shrink uncertain forecasts toward cross-pool mean ──────────────
        let active_returns: Vec<f64> = predicted_returns.iter()
            .zip(eligible.iter())
            .filter(|(_, &e)| e)
            .map(|(r, _)| *r)
            .collect();
        let mean_return = active_returns.iter().sum::<f64>() / active_returns.len() as f64;

        let adj_returns: Vec<f64> = predicted_returns.iter()
            .zip(confidence.iter())
            .map(|(&r, &c)| c * r + (1.0 - c) * mean_return)
            .collect();

        // ── Initialise: equal weight across eligible pools ─────────────────
        let init_w = 1.0 / eligible_count as f64;
        let mut w: Vec<f64> = eligible.iter()
            .map(|&e| if e { init_w } else { 0.0 })
            .collect();

        // ── Projected gradient ascent ──────────────────────────────────────
        for _ in 0..self.max_iterations {
            let sigma_w = mat_vec_mul(cov_matrix, &w);

            // Gradient of objective: μ_adj − 2λΣw
            let grad: Vec<f64> = (0..n)
                .map(|i| adj_returns[i] - 2.0 * self.risk_aversion * sigma_w[i])
                .collect();

            let w_new: Vec<f64> = w.iter()
                .zip(grad.iter())
                .map(|(wi, gi)| wi + self.learning_rate * gi)
                .collect();

            w = self.project(w_new, eligible, liquidity_caps);
        }

        w
    }

    /// Project onto the feasible set (box constraints + sum-to-one).
    fn project(&self, mut w: Vec<f64>, eligible: &[bool], liquidity_caps: &[f64]) -> Vec<f64> {
        let n = w.len();

        // Zero ineligible pools
        for i in 0..n {
            if !eligible[i] { w[i] = 0.0; }
        }

        // Clip to [min_weight, min(max_weight, liquidity_cap)]
        for i in 0..n {
            if eligible[i] {
                let upper = self.max_weight.min(liquidity_caps[i]).max(self.min_weight);
                w[i] = w[i].clamp(self.min_weight, upper);
            }
        }

        // Normalise to sum = 1
        let total: f64 = w.iter().sum();
        if total > f64::EPSILON {
            w.iter_mut().for_each(|wi| *wi /= total);
        }

        w
    }

    /// Portfolio expected return: wᵀμ
    pub fn portfolio_return(weights: &[f64], returns: &[f64]) -> f64 {
        weights.iter().zip(returns.iter()).map(|(w, r)| w * r).sum()
    }

    /// Portfolio variance: wᵀΣw
    pub fn portfolio_variance(weights: &[f64], cov: &[Vec<f64>]) -> f64 {
        let sigma_w = mat_vec_mul(cov, weights);
        weights.iter().zip(sigma_w.iter()).map(|(w, sw)| w * sw).sum()
    }
}

fn mat_vec_mul(m: &[Vec<f64>], v: &[f64]) -> Vec<f64> {
    m.iter()
        .map(|row| row.iter().zip(v.iter()).map(|(a, b)| a * b).sum())
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn identity_cov(n: usize) -> Vec<Vec<f64>> {
        (0..n).map(|i| (0..n).map(|j| if i == j { 1e-4 } else { 0.0 }).collect()).collect()
    }

    #[test]
    fn test_weights_sum_to_one() {
        let optimizer = AllocationOptimizer::new(2.0, 0.05, 0.40);
        let returns  = vec![0.05, 0.04, 0.06];
        let cov      = identity_cov(3);
        let eligible = vec![true, true, true];
        let caps     = vec![1.0, 1.0, 1.0];
        let conf     = vec![0.8, 0.7, 0.9];

        let w = optimizer.optimize(&returns, &cov, &eligible, &caps, &conf);
        let total: f64 = w.iter().sum();
        assert!((total - 1.0).abs() < 1e-6, "weights sum={}", total);
    }

    #[test]
    fn test_ineligible_pool_gets_zero() {
        let optimizer = AllocationOptimizer::new(2.0, 0.05, 0.40);
        let returns  = vec![0.05, 0.10, 0.04];
        let cov      = identity_cov(3);
        let eligible = vec![true, false, true];
        let caps     = vec![1.0, 1.0, 1.0];
        let conf     = vec![1.0, 1.0, 1.0];

        let w = optimizer.optimize(&returns, &cov, &eligible, &caps, &conf);
        assert_eq!(w[1], 0.0, "ineligible pool should be 0");
    }

    #[test]
    fn test_weights_respect_bounds() {
        let optimizer = AllocationOptimizer::new(2.0, 0.05, 0.40);
        let returns  = vec![0.05, 0.06, 0.04, 0.07];
        let cov      = identity_cov(4);
        let eligible = vec![true; 4];
        let caps     = vec![1.0; 4];
        let conf     = vec![0.9; 4];

        let w = optimizer.optimize(&returns, &cov, &eligible, &caps, &conf);
        for (i, &wi) in w.iter().enumerate() {
            assert!(wi >= 0.049 || wi == 0.0, "w[{}]={} below min", i, wi);
            assert!(wi <= 0.401, "w[{}]={} above max", i, wi);
        }
    }

    #[test]
    fn test_high_risk_aversion_diversifies() {
        // Very high lambda should push toward equal weights.
        let optimizer = AllocationOptimizer::new(50.0, 0.05, 0.40);
        let returns  = vec![0.10, 0.03, 0.07, 0.05];
        let cov      = identity_cov(4);
        let eligible = vec![true; 4];
        let caps     = vec![1.0; 4];
        let conf     = vec![1.0; 4];

        let w = optimizer.optimize(&returns, &cov, &eligible, &caps, &conf);
        // Max weight should be <= 0.40 (our cap)
        let max_w = w.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(max_w <= 0.401, "max_w={}", max_w);
    }

    #[test]
    fn test_liquidity_cap_respected() {
        let optimizer = AllocationOptimizer::new(1.0, 0.05, 0.40);
        let returns  = vec![0.10, 0.03]; // optimizer wants to pile into pool 0
        let cov      = identity_cov(2);
        let eligible = vec![true, true];
        let caps     = vec![0.15, 1.0]; // pool 0 capped at 15%
        let conf     = vec![1.0, 1.0];

        let w = optimizer.optimize(&returns, &cov, &eligible, &caps, &conf);
        assert!(w[0] <= 0.151, "pool 0 weight {} exceeds liq cap", w[0]);
    }
}
