#[derive(Clone)]
pub struct PendingSwitch {
    pub name: String,
    pub value: Option<String>,
}

impl PendingSwitch {
    pub fn flag(name: &str) -> Self {
        Self {
            name: name.into(),
            value: None,
        }
    }

    pub fn with_value(name: &str, value: &str) -> Self {
        Self {
            name: name.into(),
            value: Some(value.into()),
        }
    }
}

#[derive(Default)]
pub struct Config {
    pub pending_switches: Vec<PendingSwitch>,
}
static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();

pub fn configure(config: Config) {
    let _ = CONFIG.set(config);
}

pub fn snapshot_switches() -> Vec<PendingSwitch> {
    CONFIG
        .get()
        .map(|c| c.pending_switches.clone())
        .unwrap_or_default()
}
