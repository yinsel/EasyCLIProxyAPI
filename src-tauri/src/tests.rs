use super::*;

pub(crate) fn test_temp_dir() -> PathBuf {
    let path = std::env::temp_dir();
    // macOS temp directories may start with the system /var -> /private/var link.
    // Resolve the test root before creating fixtures; links inside fixtures remain
    // visible to the production configuration-path checks.
    #[cfg(unix)]
    let path = fs::canonicalize(path).expect("resolve test temporary directory");
    path
}

mod agent_configuration;
mod deepseek_harness_catalog;
mod agent_paths;
mod agent_state;
mod agent_transactions;
mod alias_delete;
mod alias_edit;
mod alias_edit_regressions;
mod alias_save;
mod app_settings;
mod app_update;
mod core_config;
mod core_runtime;
mod desktop_alias_routing;
#[cfg(windows)]
mod file_replace;
mod instance_lock;
mod model_aliases;
mod platform;
mod provider_health;
mod support;
