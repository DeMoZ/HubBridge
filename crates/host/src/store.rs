//! Host-side persistent state: this host's id and the agents it paired with.
//!
//! Stored as JSON with owner-only permissions in the platform config dir
//! (macOS `~/Library/Application Support/com.DeMoZ.HubBridge/host.json`,
//! Windows `%APPDATA%\DeMoZ\HubBridge\config\host.json`), or in `$HUBBRIDGE_HOME`.
//! Moving the keys to Keychain / Credential Manager is planned for step 4.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hubbridge_link::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedAgent {
    pub name: String,
    pub key: Secret,
    /// Last address that worked, used when mDNS finds nothing.
    pub last_addr: Option<String>,
    pub paired_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreData {
    pub host_id: String,
    pub host_name: String,
    #[serde(default)]
    pub agents: BTreeMap<String, PairedAgent>,
}

pub struct Store {
    path: PathBuf,
    pub data: StoreData,
}

pub fn default_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HUBBRIDGE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("com", "DeMoZ", "HubBridge")
        .map(|d| d.config_dir().to_path_buf())
        .context("no config directory for this user")
}

fn default_host_name() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .trim_end_matches(".local")
        .to_string()
}

impl Store {
    /// Load `host.json` from [`default_dir`], creating it on first use.
    pub fn load_default() -> Result<Self> {
        Self::load(&default_dir()?.join("host.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreData {
                host_id: hubbridge_link::random_id(),
                host_name: default_host_name(),
                agents: BTreeMap::new(),
            },
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let store = Store {
            path: path.to_path_buf(),
            data,
        };
        store.save()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.data)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn key_for(&self, agent_id: &str) -> Option<Secret> {
        self.data.agents.get(agent_id).map(|a| a.key.clone())
    }

    /// Find a paired agent by exact id, id prefix or case-insensitive name.
    pub fn find(&self, query: &str) -> Result<Option<(&String, &PairedAgent)>> {
        let q = query.to_lowercase();
        let hits: Vec<_> = self
            .data
            .agents
            .iter()
            .filter(|(id, a)| id.starts_with(&q) || a.name.to_lowercase() == q)
            .collect();
        match hits.len() {
            0 => Ok(None),
            1 => Ok(Some(hits[0])),
            _ => bail!("'{query}' matches {} paired agents; use a longer id", hits.len()),
        }
    }
}
