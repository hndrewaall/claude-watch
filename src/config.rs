//! Configuration structs and TOML loading.

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub general: GeneralConfig,
    pub tmux: TmuxConfig,
    pub claude: ClaudeConfig,
    pub dead_process: DeadProcessConfig,
    pub fresh_clear: FreshClearConfig,
    pub heartbeat: HeartbeatConfig,
    pub token_stall: TokenStallConfig,
    pub alerts: AlertsConfig,
    pub foreground_monitor: ForegroundMonitorConfig,
    pub watcher_monitor: WatcherMonitorConfig,
    pub context_monitor: ContextMonitorConfig,
    #[serde(default)]
    pub auto_update: AutoUpdateConfig,
    #[serde(default)]
    pub reauth: ReauthConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GeneralConfig {
    pub check_interval: u64,
    pub state_file: String,
    pub log_file: String,
    pub legacy_log_file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TmuxConfig {
    pub dashboard_pane: String,
    pub dashboard_session: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    pub max_context_tokens: u64,
    pub heartbeat_file: String,
    pub relaunch_script: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DeadProcessConfig {
    pub checks_required: u32,
    pub restart_cooldown: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FreshClearConfig {
    pub min_tokens: u64,
    pub max_tokens: u64,
    pub detections_required: u32,
    pub cooldown: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HeartbeatConfig {
    pub stale_minutes: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TokenStallConfig {
    pub checks_required: usize,
    pub max_range: u64,
    pub min_usage_fraction: f64,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct AlertsConfig {
    pub initial_cooldown: u64,
    pub escalation_tiers: Vec<u64>,
    pub max_pingme_alerts: u32,
    pub resume_prompt: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ForegroundMonitorConfig {
    pub enabled: bool,
    pub threshold_seconds: u64,
    pub check_interval: u64,
    #[serde(default = "default_interrupt_enabled")]
    pub interrupt_enabled: bool,
    #[serde(default = "default_interrupt_message")]
    pub interrupt_message: String,
    /// Maximum backoff for thinking interrupts in seconds (default: 960 = 16 min)
    #[serde(default = "default_max_thinking_backoff")]
    pub max_thinking_backoff: u64,
}

fn default_interrupt_enabled() -> bool {
    true
}

fn default_interrupt_message() -> String {
    "The foreground command was backgrounded by claude-watch because it exceeded the timeout. Use run_in_background for long commands.".to_string()
}

fn default_max_thinking_backoff() -> u64 {
    960 // 16 minutes
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WatcherMonitorConfig {
    pub enabled: bool,
    pub watchers_config: String,
    pub expected_watchmen: u32,
    /// Consecutive missing checks before injecting a restart prompt (default: 6 = ~60s)
    #[serde(default = "default_watcher_inject_threshold")]
    pub inject_threshold: u32,
    /// Cooldown in seconds between watcher-missing injections (default: 300)
    #[serde(default = "default_watcher_inject_cooldown")]
    pub inject_cooldown: u64,
}

fn default_watcher_inject_threshold() -> u32 {
    6
}

fn default_watcher_inject_cooldown() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone)]
pub struct AutoUpdateConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Minute of hour (0-59) to check for updates (default: 10)
    #[serde(default = "default_check_minute")]
    pub check_minute: u32,
    /// Minimum hours between update attempts (default: 1)
    #[serde(default = "default_cooldown_hours")]
    pub cooldown_hours: u64,
    /// Resume prompt injected after update restart
    #[serde(default = "default_update_resume_prompt")]
    pub resume_prompt: String,
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_minute: default_check_minute(),
            cooldown_hours: default_cooldown_hours(),
            resume_prompt: default_update_resume_prompt(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ReauthConfig {
    #[serde(default = "default_reauth_enabled")]
    pub enabled: bool,
    /// Interval between repeated reauth alerts in seconds (default: 10800 = 3 hours)
    #[serde(default = "default_reauth_alert_interval")]
    pub alert_interval_seconds: u64,
}

impl Default for ReauthConfig {
    fn default() -> Self {
        Self {
            enabled: default_reauth_enabled(),
            alert_interval_seconds: default_reauth_alert_interval(),
        }
    }
}

fn default_reauth_enabled() -> bool {
    true
}

fn default_reauth_alert_interval() -> u64 {
    10800 // 3 hours
}

fn default_check_minute() -> u32 {
    10
}

fn default_cooldown_hours() -> u64 {
    1
}

fn default_update_resume_prompt() -> String {
    "resume".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextMonitorConfig {
    pub enabled: bool,
    /// Token percentage threshold (legacy fallback, ignored if threshold_margin is set)
    #[serde(default = "default_threshold_percent")]
    pub threshold_percent: u64,
    /// Fixed token margin from max_context_tokens at which to trigger (e.g., 30000 = trigger at max - 30K)
    /// When set, overrides threshold_percent.
    #[serde(default)]
    pub threshold_margin: Option<u64>,
    /// Compact remaining % at which to trigger (primary trigger)
    pub compact_trigger_percent: u32,
    /// Grace period before forced self-clear (seconds)
    pub grace_period: u64,
    /// Minimum interval between context clear triggers (seconds)
    pub cooldown: u64,
}

fn default_threshold_percent() -> u64 {
    75
}

/// Load config from well-known paths or CLAUDE_WATCH_CONFIG env var.
pub fn load_config() -> Config {
    let config_paths = [
        std::env::var("CLAUDE_WATCH_CONFIG").unwrap_or_default(),
        format!(
            "{}/.config/claude-watch/config.toml",
            std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string())
        ),
        "config.toml".to_string(),  // fallback: look in current directory
    ];

    for path in &config_paths {
        if path.is_empty() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            match toml::from_str::<Config>(&content) {
                Ok(config) => {
                    tracing::info!(path, "loaded config");
                    return config;
                }
                Err(e) => {
                    eprintln!("Failed to parse config {}: {}", path, e);
                }
            }
        }
    }
    eprintln!("FATAL: no config file found. Tried: {:?}", config_paths);
    std::process::exit(1);
}

/// Parse config from a TOML string. Useful for testing.
#[cfg(test)]
pub fn parse_config(toml_str: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONFIG: &str = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[token_stall]
checks_required = 5
max_range = 100
min_usage_fraction = 0.5

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10
interrupt_enabled = true
interrupt_message = "Test interrupt message"

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300

[auto_update]
enabled = false
check_minute = 10
cooldown_hours = 1
resume_prompt = "resume"

[reauth]
enabled = true
alert_interval_seconds = 10800
"#;

    #[test]
    fn test_parse_valid_config() {
        let config = parse_config(SAMPLE_CONFIG).expect("should parse valid config");
        assert_eq!(config.general.check_interval, 10);
        assert_eq!(config.tmux.dashboard_pane, "dashboard:0.0");
        assert_eq!(config.claude.max_context_tokens, 200000);
        assert_eq!(config.dead_process.checks_required, 3);
        assert_eq!(config.fresh_clear.min_tokens, 1000);
        assert_eq!(config.heartbeat.stale_minutes, 15);
        assert_eq!(config.token_stall.checks_required, 5);
        assert_eq!(config.alerts.escalation_tiers.len(), 5);
        assert!(config.foreground_monitor.enabled);
        assert!(config.watcher_monitor.enabled);
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
    }

    #[test]
    fn test_parse_minimal_values() {
        let config = parse_config(SAMPLE_CONFIG).unwrap();
        assert_eq!(config.token_stall.min_usage_fraction, 0.5);
        assert_eq!(config.alerts.max_pingme_alerts, 3);
        assert_eq!(config.alerts.resume_prompt, "Resume your work.");
        assert!(!config.auto_update.enabled);
        assert_eq!(config.auto_update.check_minute, 10);
        assert_eq!(config.auto_update.cooldown_hours, 1);
        assert_eq!(config.auto_update.resume_prompt, "resume");
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
    }

    #[test]
    fn test_parse_config_without_auto_update_section() {
        // Config without [auto_update] should still parse with defaults
        let config_without_auto_update = r#"
[general]
check_interval = 10
state_file = "/tmp/test-state.json"
log_file = "/tmp/test.jsonl"
legacy_log_file = "/tmp/test.log"

[tmux]
dashboard_pane = "dashboard:0.0"
dashboard_session = "dashboard"

[claude]
max_context_tokens = 200000
heartbeat_file = "/tmp/heartbeat"
relaunch_script = "/tmp/relaunch.sh"

[dead_process]
checks_required = 3
restart_cooldown = 300

[fresh_clear]
min_tokens = 1000
max_tokens = 50000
detections_required = 2
cooldown = 120

[heartbeat]
stale_minutes = 15

[token_stall]
checks_required = 5
max_range = 100
min_usage_fraction = 0.5

[alerts]
initial_cooldown = 60
escalation_tiers = [60, 120, 300, 600, 3600]
max_pingme_alerts = 3
resume_prompt = "Resume your work."

[foreground_monitor]
enabled = true
threshold_seconds = 120
check_interval = 10

[watcher_monitor]
enabled = true
watchers_config = "/tmp/watchers.conf"
expected_watchmen = 3

[context_monitor]
enabled = true
threshold_percent = 75
compact_trigger_percent = 5
grace_period = 120
cooldown = 300
"#;
        let config = parse_config(config_without_auto_update)
            .expect("should parse without [auto_update] section");
        // Defaults should be applied
        assert!(!config.auto_update.enabled);
        assert_eq!(config.auto_update.check_minute, 10);
        assert_eq!(config.auto_update.cooldown_hours, 1);
        assert_eq!(config.auto_update.resume_prompt, "resume");
        // reauth defaults should also be applied
        assert!(config.reauth.enabled);
        assert_eq!(config.reauth.alert_interval_seconds, 10800);
    }

    #[test]
    fn test_parse_invalid_config() {
        let result = parse_config("not valid toml [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_section() {
        let partial = r#"
[general]
check_interval = 10
state_file = "/tmp/s"
log_file = "/tmp/l"
legacy_log_file = "/tmp/ll"
"#;
        let result = parse_config(partial);
        assert!(result.is_err(), "missing sections should fail");
    }
}
