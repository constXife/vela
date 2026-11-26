// SPDX-License-Identifier: MIT

//! Non-secret multi-account config (`~/.config/vela/config.toml`) and OS Keyring
//! secret storage (`org.freedesktop.secrets` via `oo7`).

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vela_core::VelaError;
use zeroize::Zeroizing;

use crate::{JmapConfig, JmapCredentials};

const KEYRING_APP_ID: &str = "com.rlyeh.vela.Cosmic";
const KEYRING_LABEL: &str = "Vela JMAP Account";
const CACHE_KEY_LABEL: &str = "Vela Local Cache Key";

/// Non-secret metadata for a single JMAP account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    #[serde(default)]
    pub id: String,
    pub server_url: String,
    pub username: String,
    #[serde(default)]
    pub auth_kind: AuthKind,
}

impl AccountConfig {
    pub fn ensure_id(&mut self) {
        if self.id.trim().is_empty() {
            self.id = account_id_for(&self.server_url, &self.username);
        }
    }

    pub fn host(&self) -> String {
        self.server_url
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string()
    }
}

/// Multi-account configuration stored in `~/.config/vela/config.toml` (`0600`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsConfig {
    #[serde(default)]
    pub active_account_id: Option<String>,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

impl AccountsConfig {
    pub fn active(&self) -> Option<&AccountConfig> {
        if let Some(id) = &self.active_account_id
            && let Some(acc) = self.accounts.iter().find(|a| &a.id == id)
        {
            return Some(acc);
        }
        self.accounts.first()
    }

    pub fn find(&self, id: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn upsert(&mut self, mut account: AccountConfig) -> AccountConfig {
        account.ensure_id();
        if let Some(existing) = self.accounts.iter_mut().find(|a| {
            a.id == account.id
                || (a.server_url == account.server_url && a.username == account.username)
        }) {
            *existing = account.clone();
        } else {
            self.accounts.push(account.clone());
        }
        self.active_account_id = Some(account.id.clone());
        account
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    Basic,
    Bearer,
}

/// Deterministic, filesystem-safe identifier for a `(server_url, username)` pair.
pub fn account_id_for(server_url: &str, username: &str) -> String {
    let host = server_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let raw = format!("{username}_{host}");
    let slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "default".to_string()
    } else {
        slug
    }
}

fn config_path() -> Result<PathBuf, VelaError> {
    let base = dirs::config_dir()
        .ok_or_else(|| VelaError::Other("unable to determine XDG config directory".into()))?;
    Ok(base.join("vela").join("config.toml"))
}

/// Parse `config.toml`, transparently upgrading legacy single-account files.
pub fn parse_accounts_config(raw: &str) -> Result<AccountsConfig, VelaError> {
    if let Ok(mut multi) = toml::from_str::<AccountsConfig>(raw)
        && !multi.accounts.is_empty()
    {
        for acc in &mut multi.accounts {
            acc.ensure_id();
        }
        if multi.active_account_id.is_none() {
            multi.active_account_id = multi.accounts.first().map(|a| a.id.clone());
        }
        return Ok(multi);
    }

    if let Ok(mut legacy) = toml::from_str::<AccountConfig>(raw) {
        legacy.ensure_id();
        let active_id = legacy.id.clone();
        return Ok(AccountsConfig {
            active_account_id: Some(active_id),
            accounts: vec![legacy],
        });
    }

    Err(VelaError::Other("invalid config.toml format".into()))
}

/// Read all saved accounts from `~/.config/vela/config.toml`.
pub fn load_accounts_config() -> Result<AccountsConfig, VelaError> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AccountsConfig::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| VelaError::Other(format!("failed to read {}: {err}", path.display())))?;
    if raw.trim().is_empty() {
        return Ok(AccountsConfig::default());
    }
    parse_accounts_config(&raw)
}

/// Read the active account from `~/.config/vela/config.toml` if present.
pub fn load_account_config() -> Result<Option<AccountConfig>, VelaError> {
    Ok(load_accounts_config()?.active().cloned())
}

/// Atomically write `~/.config/vela/config.toml` with owner-only permissions.
pub fn save_accounts_config(cfg: &AccountsConfig) -> Result<(), VelaError> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            VelaError::Other(format!("failed to create {}: {err}", parent.display()))
        })?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|err| {
            VelaError::Other(format!(
                "failed to set permissions on {}: {err}",
                parent.display()
            ))
        })?;
    }

    let serialized = toml::to_string_pretty(cfg)
        .map_err(|err| VelaError::Other(format!("failed to serialize config: {err}")))?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, serialized)
        .map_err(|err| VelaError::Other(format!("failed to write {}: {err}", tmp.display())))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|err| {
        VelaError::Other(format!(
            "failed to set permissions on {}: {err}",
            tmp.display()
        ))
    })?;
    fs::rename(&tmp, &path)
        .map_err(|err| VelaError::Other(format!("failed to commit {}: {err}", path.display())))?;
    Ok(())
}

/// Set the active account ID in `config.toml`.
pub fn set_active_account(account_id: &str) -> Result<(), VelaError> {
    let mut cfg = load_accounts_config()?;
    if cfg.find(account_id).is_some() {
        cfg.active_account_id = Some(account_id.to_string());
        save_accounts_config(&cfg)?;
    }
    Ok(())
}

fn keyring_attributes<'a>(server_url: &'a str, username: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([
        ("application", KEYRING_APP_ID),
        ("server", server_url),
        ("username", username),
    ])
}

/// Load saved JMAP config + secret for the active account from the OS Keyring.
pub async fn load_saved_jmap_config() -> Result<Option<JmapConfig>, VelaError> {
    let Some(account) = load_account_config()? else {
        return Ok(None);
    };
    load_jmap_config_for_account(&account).await
}

/// Load saved JMAP config + secret for a specific account ID from the OS Keyring.
pub async fn load_jmap_config_for(account_id: &str) -> Result<Option<JmapConfig>, VelaError> {
    let cfg = load_accounts_config()?;
    let Some(account) = cfg.find(account_id).cloned() else {
        return Ok(None);
    };
    load_jmap_config_for_account(&account).await
}

async fn load_jmap_config_for_account(
    account: &AccountConfig,
) -> Result<Option<JmapConfig>, VelaError> {
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = keyring_attributes(&account.server_url, &account.username);
    let items = keyring
        .search_items(&attrs)
        .await
        .map_err(|err| VelaError::Auth(format!("keyring lookup failed: {err}")))?;

    let Some(item) = items.into_iter().next() else {
        return Ok(None);
    };
    let secret_bytes = item
        .secret()
        .await
        .map_err(|err| VelaError::Auth(format!("failed to read secret from keyring: {err}")))?;
    let secret = String::from_utf8(secret_bytes.to_vec())
        .map_err(|_| VelaError::Auth("stored keyring secret is not valid UTF-8".into()))?;

    let credentials = match account.auth_kind {
        AuthKind::Basic => JmapCredentials::Basic {
            username: account.username.clone(),
            password: secret,
        },
        AuthKind::Bearer => JmapCredentials::Bearer { token: secret },
    };

    Ok(Some(JmapConfig {
        server_url: account.server_url.clone(),
        credentials,
    }))
}

/// Save non-secret account metadata into `config.toml` (upserting into the
/// multi-account list and making it active) and store the secret in OS Keyring.
pub async fn save_jmap_config(config: &JmapConfig) -> Result<AccountConfig, VelaError> {
    let (username, secret, auth_kind) = match &config.credentials {
        JmapCredentials::Basic { username, password } => {
            (username.as_str(), password.as_str(), AuthKind::Basic)
        }
        JmapCredentials::Bearer { token } => ("bearer", token.as_str(), AuthKind::Bearer),
    };

    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = keyring_attributes(&config.server_url, username);
    keyring
        .create_item(KEYRING_LABEL, &attrs, secret.as_bytes(), true)
        .await
        .map_err(|err| VelaError::Auth(format!("failed to save secret to OS keyring: {err}")))?;

    let mut accounts_cfg = load_accounts_config().unwrap_or_default();
    let saved = accounts_cfg.upsert(AccountConfig {
        id: account_id_for(&config.server_url, username),
        server_url: config.server_url.clone(),
        username: username.to_string(),
        auth_kind,
    });
    save_accounts_config(&accounts_cfg)?;
    Ok(saved)
}

/// Remove a specific account's secret & cache key from OS Keyring and remove it
/// from `config.toml`, returning the next remaining active account ID if any.
pub async fn remove_account(account_id: &str) -> Result<Option<String>, VelaError> {
    let mut cfg = load_accounts_config()?;
    if let Some(pos) = cfg.accounts.iter().position(|a| a.id == account_id) {
        let removed = cfg.accounts.remove(pos);
        if let Ok(keyring) = oo7::Keyring::new().await {
            let attrs = keyring_attributes(&removed.server_url, &removed.username);
            let _ = keyring.delete(&attrs).await;
        }
        let _ = clear_cache_key_for(&removed.id).await;
    }
    let next_active = cfg.accounts.first().map(|a| a.id.clone());
    cfg.active_account_id = next_active.clone();
    save_accounts_config(&cfg)?;
    Ok(next_active)
}

/// Remove stored secret for the active account from OS Keyring.
pub async fn clear_saved_secret() -> Result<(), VelaError> {
    let Some(account) = load_account_config()? else {
        return Ok(());
    };
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = keyring_attributes(&account.server_url, &account.username);
    keyring
        .delete(&attrs)
        .await
        .map_err(|err| VelaError::Auth(format!("failed to clear keyring secret: {err}")))?;
    Ok(())
}

fn cache_key_attributes(account_id: &str) -> HashMap<&'static str, String> {
    HashMap::from([
        ("application", KEYRING_APP_ID.to_string()),
        ("kind", "sqlcipher_cache_key".to_string()),
        ("account_id", account_id.to_string()),
    ])
}

/// Load or create the 256-bit hex encryption key for the active account's cache.
pub async fn load_or_create_cache_key() -> Result<Zeroizing<String>, VelaError> {
    let account_id = load_account_config()?
        .map(|a| a.id)
        .unwrap_or_else(|| "default".to_string());
    load_or_create_cache_key_for(&account_id).await
}

/// Load or create the 256-bit hex encryption key for a specific account's cache.
pub async fn load_or_create_cache_key_for(
    account_id: &str,
) -> Result<Zeroizing<String>, VelaError> {
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = cache_key_attributes(account_id);
    let items = keyring
        .search_items(&attrs)
        .await
        .map_err(|err| VelaError::Auth(format!("keyring cache-key lookup failed: {err}")))?;

    if let Some(item) = items.into_iter().next()
        && let Ok(secret_bytes) = item.secret().await
        && let Ok(existing) = String::from_utf8(secret_bytes.to_vec())
        && existing.len() == 64
        && existing.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(Zeroizing::new(existing));
    }

    let generated = generate_random_hex_key()?;
    keyring
        .create_item(CACHE_KEY_LABEL, &attrs, generated.as_bytes(), true)
        .await
        .map_err(|err| VelaError::Auth(format!("failed to save cache key to OS keyring: {err}")))?;
    Ok(generated)
}

/// Delete the local cache encryption key for a specific account from OS Keyring.
pub async fn clear_cache_key_for(account_id: &str) -> Result<(), VelaError> {
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = cache_key_attributes(account_id);
    keyring
        .delete(&attrs)
        .await
        .map_err(|err| VelaError::Auth(format!("failed to clear cache key: {err}")))?;
    Ok(())
}

/// Delete the local cache encryption key for the active account from OS Keyring.
pub async fn clear_cache_key() -> Result<(), VelaError> {
    let account_id = load_account_config()?
        .map(|a| a.id)
        .unwrap_or_else(|| "default".to_string());
    clear_cache_key_for(&account_id).await
}

fn generate_random_hex_key() -> Result<Zeroizing<String>, VelaError> {
    let mut raw = Zeroizing::new([0u8; 32]);
    let mut urandom = fs::File::open("/dev/urandom")
        .map_err(|err| VelaError::Other(format!("failed to open /dev/urandom: {err}")))?;
    urandom
        .read_exact(&mut *raw)
        .map_err(|err| VelaError::Other(format!("failed to read random key bytes: {err}")))?;

    let mut hex = Zeroizing::new(String::with_capacity(64));
    for byte in raw.iter() {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_single_account_config() {
        let legacy = r#"
server_url = "https://mail.example.com"
username = "constxife"
auth_kind = "basic"
"#;
        let parsed = parse_accounts_config(legacy).unwrap();
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.active().unwrap().username, "constxife");
        assert_eq!(parsed.active().unwrap().id, "constxife_yogg_rlyeh_void");
    }

    #[test]
    fn multi_account_upsert_and_switch() {
        let mut cfg = AccountsConfig::default();
        let a1 = cfg.upsert(AccountConfig {
            id: String::new(),
            server_url: "https://mail.example.com".into(),
            username: "work".into(),
            auth_kind: AuthKind::Basic,
        });
        let a2 = cfg.upsert(AccountConfig {
            id: String::new(),
            server_url: "https://api.fastmail.com".into(),
            username: "personal".into(),
            auth_kind: AuthKind::Bearer,
        });
        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.active().unwrap().id, a2.id);

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let reparsed = parse_accounts_config(&serialized).unwrap();
        assert_eq!(reparsed.accounts.len(), 2);
        assert_eq!(reparsed.find(&a1.id).unwrap().host(), "mail.example.com");
    }
}
