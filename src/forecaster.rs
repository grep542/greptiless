
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AR1Model {
    /// OLS intercept.
    pub alpha: f64,
    /// Autoregressive coefficient (typically 0.85–0.97 for DeFi rates).
    pub beta: f64,
    /// Residual standard deviation.
    pub sigma: f64,
    /// Long-run mean-reversion level = α / (1 − β).
    pub mean_reversion_level: f64,
}

impl AR1Model {
    pub fn fit(observations: &[f64]) -> Option<Self> {
        const MIN_OBS: usize = 10;
        let n = observations.len();
        if n < MIN_OBS {
            return None;
        }

        // y[t]   = observations[1..n]
        // y[t-1] = observations[0..n-1]
        let y: &[f64] = &observations[1..];
        let x: &[f64] = &observations[..n - 1];
        let m = y.len() as f64;

        let x_mean = x.iter().sum::<f64>() / m;
        let y_mean = y.iter().sum::<f64>() / m;

        let cov_xy: f64 = x.iter().zip(y.iter())
            .map(|(xi, yi)| (xi - x_mean) * (yi - y_mean))
            .sum::<f64>();
        let var_x: f64 = x.iter()
            .map(|xi| (xi - x_mean).powi(2))
            .sum::<f64>();

        if var_x.abs() < f64::EPSILON {
            return None;
        }

        let beta  = (cov_xy / var_x).clamp(-0.999, 0.999); // keep stationary
        let alpha = y_mean - beta * x_mean;

        let sigma = {
            let ss: f64 = x.iter().zip(y.iter())
                .map(|(xi, yi)| (yi - (alpha + beta * xi)).powi(2))
                .sum::<f64>();
            (ss / m).sqrt()
        };

        // Guard: if beta == 1 we'd divide by zero; already clamped above.
        let mean_reversion_level = if (1.0 - beta).abs() > f64::EPSILON {
            alpha / (1.0 - beta)
        } else {
            x_mean
        };

        Some(AR1Model { alpha, beta, sigma, mean_reversion_level })
    }

    pub fn forecast(&self, current_apy: f64, horizon_days: u32) -> (f64, f64) {
        let h = horizon_days as i32;

        let beta_h = self.beta.powi(h);
        let point = if (1.0 - self.beta).abs() > f64::EPSILON {
            self.alpha * (1.0 - beta_h) / (1.0 - self.beta) + beta_h * current_apy
        } else {
            current_apy
        };

        let variance: f64 = (0..h)
            .map(|k| self.sigma.powi(2) * self.beta.powi(2 * k))
            .sum();

        let ci = 1.96 * variance.sqrt();
        (point.max(0.0), ci)
    }

    pub fn confidence_weight(&self, current_apy: f64, horizon_days: u32) -> f64 {
        let (forecast, ci) = self.forecast(current_apy, horizon_days);
        if forecast.abs() < f64::EPSILON {
            return 0.0;
        }
        (1.0 - (ci / forecast).min(1.0)).max(0.0)
    }
}

pub struct CovarianceMatrix {
    pub pool_ids: Vec<String>,
    /// Row-major n×n matrix.
    pub matrix: Vec<Vec<f64>>,
}

impl CovarianceMatrix {
    pub fn compute(history: &HashMap<String, Vec<f64>>) -> Self {
        let pool_ids: Vec<String> = history.keys().cloned().collect();
        let n = pool_ids.len();
        let mut matrix = vec![vec![0.0_f64; n]; n];

        for (i, id_i) in pool_ids.iter().enumerate() {
            for (j, id_j) in pool_ids.iter().enumerate() {
                let xi = &history[id_i];
                let xj = &history[id_j];
                let len = xi.len().min(xj.len());
                if len < 2 {
                    matrix[i][j] = if i == j { 1e-6 } else { 0.0 };
                    continue;
                }
                let xi = &xi[xi.len() - len..];
                let xj = &xj[xj.len() - len..];
                let m = len as f64;

                let mean_i = xi.iter().sum::<f64>() / m;
                let mean_j = xj.iter().sum::<f64>() / m;

                let cov: f64 = xi.iter().zip(xj.iter())
                    .map(|(a, b)| (a - mean_i) * (b - mean_j))
                    .sum::<f64>() / m;

                matrix[i][j] = cov;
            }
        }

        CovarianceMatrix { pool_ids, matrix }
    }

    pub fn regularize(&mut self, epsilon: f64) {
        let n = self.pool_ids.len();
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    self.matrix[i][j] *= 1.0 - epsilon;
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_ar1(alpha: f64, beta: f64, sigma: f64, n: usize) -> Vec<f64> {
        // Deterministic residuals for reproducibility (no rand dep).
        let mut series = vec![alpha / (1.0 - beta)];
        for i in 1..n {
            // pseudo-noise: alternating small perturbation
            let noise = if i % 2 == 0 { sigma * 0.5 } else { -sigma * 0.5 };
            let next = alpha + beta * series[i - 1] + noise;
            series.push(next.max(0.0));
        }
        series
    }

    #[test]
    fn test_ar1_fit_recovers_params() {
        // True params: alpha=0.001, beta=0.95
        let obs = synthetic_ar1(0.001, 0.95, 0.0001, 100);
        let model = AR1Model::fit(&obs).expect("should fit");
        // Beta should be close to 0.95 (within 0.1 given deterministic noise)
        assert!((model.beta - 0.95).abs() < 0.1, "beta={}", model.beta);
        assert!(model.alpha >= 0.0);
    }

    #[test]
    fn test_forecast_is_bounded() {
        let obs = synthetic_ar1(0.001, 0.90, 0.0002, 50);
        let model = AR1Model::fit(&obs).unwrap();
        let current = *obs.last().unwrap();
        let (forecast, ci) = model.forecast(current, 7);
        assert!(forecast >= 0.0, "forecast must be non-negative");
        assert!(ci >= 0.0, "CI must be non-negative");
    }

    #[test]
    fn test_confidence_weight_range() {
        let obs = synthetic_ar1(0.002, 0.93, 0.0001, 60);
        let model = AR1Model::fit(&obs).unwrap();
        let current = *obs.last().unwrap();
        let w = model.confidence_weight(current, 7);
        assert!((0.0..=1.0).contains(&w), "weight={}", w);
    }

    #[test]
    fn test_insufficient_history_returns_none() {
        let obs = vec![0.05, 0.051, 0.049]; // only 3 points
        assert!(AR1Model::fit(&obs).is_none());
    }

    #[test]
    fn test_covariance_diagonal_positive() {
        let mut history = HashMap::new();
        history.insert("pool-a".to_string(), synthetic_ar1(0.001, 0.90, 0.0001, 30));
        history.insert("pool-b".to_string(), synthetic_ar1(0.002, 0.85, 0.0002, 30));
        let cov = CovarianceMatrix::compute(&history);
        for i in 0..cov.pool_ids.len() {
            assert!(cov.matrix[i][i] >= 0.0, "diagonal must be non-negative");
        }
    }
}
