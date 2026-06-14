//! OS keyring integration for secure credential storage.
//!
//! Stores and retrieves VPN credentials using the platform's native
//! credential store (macOS Keychain, Linux Secret Service).
//! The service name is `"vpn-jumphost"` and entries are keyed by
//! `"username"` and `"password"`.
//!
//! Uses `keyring-core` v4 with platform-specific store crates:
//! - macOS: `apple-native-keyring-store` (Keychain)
//! - Linux: `dbus-secret-service-keyring-store` (Secret Service / GNOME Keyring)

use std::sync::OnceLock;

use tracing::{debug, warn};

const SERVICE: &str = "vpn-jumphost";
const KEY_USERNAME: &str = "username";
const KEY_PASSWORD: &str = "password";

/// Ensure the platform-appropriate default credential store is set up
/// exactly once. Returns `Ok(())` on success, or an error string if
/// store initialisation failed.
fn ensure_store() -> Result<(), String> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT.get_or_init(|| init_store().map_err(|e| e.to_string())).clone()
}

/// Platform-specific store initialisation.
fn init_store() -> Result<(), keyring_core::Error> {
    #[cfg(target_os = "macos")]
    {
        let store = apple_native_keyring_store::keychain::Store::new()?;
        keyring_core::set_default_store(store);
    }
    #[cfg(target_os = "linux")]
    {
        let store = dbus_secret_service_keyring_store::Store::new()?;
        keyring_core::set_default_store(store);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err(keyring_core::Error::NoDefaultStore(
            "no keyring store available on this platform".into(),
        ));
    }
    Ok(())
}

/// Store VPN credentials in the OS keyring.
pub fn store_credentials(username: &str, password: &str) -> anyhow::Result<()> {
    ensure_store().map_err(|e| anyhow::anyhow!("keyring store init failed: {e}"))?;

    let user_entry = keyring_core::Entry::new(SERVICE, KEY_USERNAME)?;
    user_entry.set_password(username)?;
    debug!("stored username in OS keyring");

    let pass_entry = keyring_core::Entry::new(SERVICE, KEY_PASSWORD)?;
    pass_entry.set_password(password)?;
    debug!("stored password in OS keyring");

    Ok(())
}

/// Read a single non-empty credential from the OS keyring.
///
/// Returns `Some(value)` when the entry exists and is non-empty,
/// `None` otherwise (with appropriate debug/warn logging).
fn read_entry(key: &str) -> Option<String> {
    match keyring_core::Entry::new(SERVICE, key) {
        Ok(entry) => match entry.get_password() {
            Ok(val) if !val.is_empty() => Some(val),
            Ok(_) => {
                debug!(key, "keyring: entry is empty");
                None
            }
            Err(keyring_core::Error::NoEntry) => {
                debug!(key, "keyring: no entry");
                None
            }
            Err(e) => {
                warn!(key, error = %e, "keyring: could not read entry");
                None
            }
        },
        Err(e) => {
            warn!(key, error = %e, "keyring: could not open entry");
            None
        }
    }
}

/// Retrieve VPN credentials from the OS keyring.
///
/// Returns `Some((username, password))` if both are present and
/// non-empty, `None` otherwise.
pub fn get_credentials() -> Option<(String, String)> {
    if let Err(e) = ensure_store() {
        debug!(error = %e, "keyring store not available");
        return None;
    }

    let username = read_entry(KEY_USERNAME)?;
    let password = read_entry(KEY_PASSWORD)?;

    Some((username, password))
}

/// Delete VPN credentials from the OS keyring.
///
/// Silently ignores missing entries.
pub fn delete_credentials() -> anyhow::Result<()> {
    ensure_store().map_err(|e| anyhow::anyhow!("keyring store init failed: {e}"))?;

    for key in [KEY_USERNAME, KEY_PASSWORD] {
        match keyring_core::Entry::new(SERVICE, key) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) => debug!(key, "deleted keyring entry"),
                Err(keyring_core::Error::NoEntry) => {
                    debug!(key, "keyring entry not found (nothing to delete)");
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("failed to delete keyring entry '{key}': {e}"));
                }
            },
            Err(e) => {
                return Err(anyhow::anyhow!("failed to open keyring entry '{key}': {e}"));
            }
        }
    }
    Ok(())
}
