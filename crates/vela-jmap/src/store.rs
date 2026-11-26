// SPDX-License-Identifier: MIT

//! Non-secret account config (`~/.config/vela/config.toml`) and OS Keyring
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

/// Non-secret account metadata stored on disk in `config.toml` (`0600`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountConfig {
    pub server_url: String,
    pub username: String,
    #[serde(default)]
    pub auth_kind: AuthKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    Basic,
    Bearer,
}

fn config_path() -> Result<PathBuf, VelaError> {
    let base = dirs::config_dir()
        .ok_or_else(|| VelaError::Other("unable to determine XDG config directory".into()))?;
    Ok(base.join("vela").join("config.toml"))
}

/// Read `~/.config/vela/config.toml` if present.
pub fn load_account_config() -> Result<Option<AccountConfig>, VelaError> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| VelaError::Other(format!("failed to read {}: {err}", path.display())))?;
    let cfg: AccountConfig = toml::from_str(&raw)
        .map_err(|err| VelaError::Other(format!("invalid {}: {err}", path.display())))?;
    Ok(Some(cfg))
}

/// Atomically write `~/.config/vela/config.toml` with owner-only permissions.
pub fn save_account_config(cfg: &AccountConfig) -> Result<(), VelaError> {
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

fn keyring_attributes<'a>(server_url: &'a str, username: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([
        ("application", KEYRING_APP_ID),
        ("server", server_url),
        ("username", username),
    ])
}

/// Load saved account config + secret from the OS Keyring.
pub async fn load_saved_jmap_config() -> Result<Option<JmapConfig>, VelaError> {
    let Some(account) = load_account_config()? else {
        return Ok(None);
    };

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
            username: account.username,
            password: secret,
        },
        AuthKind::Bearer => JmapCredentials::Bearer { token: secret },
    };

    Ok(Some(JmapConfig {
        server_url: account.server_url,
        credentials,
    }))
}

/// Save non-secret account metadata to `config.toml` and secret to OS Keyring.
pub async fn save_jmap_config(config: &JmapConfig) -> Result<(), VelaError> {
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

    save_account_config(&AccountConfig {
        server_url: config.server_url.clone(),
        username: username.to_string(),
        auth_kind,
    })?;
    Ok(())
}

/// Remove stored secret from OS Keyring (keeps `server_url` / `username` in
/// `config.toml` so re-signing in is convenient).
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

fn cache_key_attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("application", KEYRING_APP_ID),
        ("kind", "sqlcipher_cache_key"),
    ])
}

/// Load the 256-bit hex encryption key for `cache.db` from the OS Keyring,
/// generating and persisting a fresh random key on first run.
pub async fn load_or_create_cache_key() -> Result<Zeroizing<String>, VelaError> {
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = cache_key_attributes();
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

/// Delete the local cache encryption key from the OS Keyring (crypto-shredding on Sign out).
pub async fn clear_cache_key() -> Result<(), VelaError> {
    let keyring = oo7::Keyring::new()
        .await
        .map_err(|err| VelaError::Auth(format!("OS keyring unavailable: {err}")))?;
    let attrs = cache_key_attributes();
    keyring
        .delete(&attrs)
        .await
        .map_err(|err| VelaError::Auth(format!("failed to clear cache key: {err}")))?;
    Ok(())
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
