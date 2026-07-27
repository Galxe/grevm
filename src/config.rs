use crate::DelegatedSafetyConfig;

/// Runtime configuration for one grevm scheduler.
///
/// Environment variables are read once when [`Self::from_env`] is called. Callers that need
/// consensus-stable behavior should construct this value explicitly and pass it to
/// [`crate::Scheduler::new_with_runtime_config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrevmConfig {
    /// Number of speculative execution workers.
    pub concurrency_level: usize,
    /// Execute the whole block through the sequential path.
    pub force_sequential: bool,
    /// Blocks smaller than this threshold use the sequential path.
    pub min_parallel_txs: usize,
    /// EIP-7702 delegated-account safety policy.
    pub delegated_safety: DelegatedSafetyConfig,
}

impl GrevmConfig {
    /// Builds the Grevm runtime configuration from environment variables.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            concurrency_level: env_or("GREVM_CONCURRENT_LEVEL", defaults.concurrency_level),
            force_sequential: env_or("GREVM_FALLBACK_SEQUENTIAL", defaults.force_sequential),
            min_parallel_txs: env_or("GREVM_MIN_PARALLEL_TXS", defaults.min_parallel_txs),
            delegated_safety: defaults.delegated_safety,
        }
    }

    /// Overrides the delegated-account policy while preserving all execution settings.
    pub fn with_delegated_safety(mut self, delegated_safety: DelegatedSafetyConfig) -> Self {
        self.delegated_safety = delegated_safety;
        self
    }
}

impl Default for GrevmConfig {
    fn default() -> Self {
        Self {
            concurrency_level: std::thread::available_parallelism().map_or(8, |value| value.get()),
            force_sequential: false,
            min_parallel_txs: 64,
            delegated_safety: DelegatedSafetyConfig::default(),
        }
    }
}

fn env_or<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_is_safe_and_bounded() {
        let config = GrevmConfig::default();
        assert!(config.concurrency_level > 0);
        assert_eq!(config.min_parallel_txs, 64);
        assert!(!config.force_sequential);
        assert!(!config.delegated_safety.forbid_delegated_create);
        assert!(!config.delegated_safety.reserve_delegated_balance);
    }

    #[test]
    fn delegated_policy_builder_preserves_scheduler_settings() {
        let config = GrevmConfig {
            concurrency_level: 3,
            force_sequential: true,
            min_parallel_txs: 7,
            delegated_safety: DelegatedSafetyConfig::default(),
        }
        .with_delegated_safety(DelegatedSafetyConfig::enabled());

        assert_eq!(config.concurrency_level, 3);
        assert!(config.force_sequential);
        assert_eq!(config.min_parallel_txs, 7);
        assert!(config.delegated_safety.forbid_delegated_create);
        assert!(config.delegated_safety.reserve_delegated_balance);
    }
}
