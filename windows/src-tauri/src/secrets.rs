// API keys live in the Windows Credential Manager or, on Linux, the Secret
// Service (KWallet on KDE, GNOME Keyring on GNOME) — never on disk and never in
// the front end. The island can only ask whether a key is present.

use keyring::Entry;

const SERVICE: &str = "fr.louisraille.coucou";

/// Every key Coucou may store. Anything outside this list is refused.
pub const KNOWN_KEYS: &[&str] = &[
    "anthropic-api-key",
    "openai-api-key",
    "gemini-api-key",
    "n8n-url",
    "n8n-api-key",
    "vercel-token",
    "github-token",
    "stripe-api-key",
    "resend-api-key",
    "notion-api-key",
    "calcom-api-key",
];

/// API keys that may come in several, from different accounts: the main one
/// and `<name>-2` up to `<name>-5`, tried in turn when one runs out of quota.
const MULTI_ACCOUNT: &[&str] = &["anthropic-api-key", "openai-api-key", "gemini-api-key"];
pub const MAX_ACCOUNTS: u8 = 5;

fn known(key: &str) -> bool {
    KNOWN_KEYS.contains(&key)
        || MULTI_ACCOUNT.iter().any(|base| {
            key.strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('-'))
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| (2..=MAX_ACCOUNTS).contains(&n))
        })
}

/// The name an account's key is stored under: 1 is the main key.
pub fn account_key(base: &str, account: u8) -> String {
    if account <= 1 {
        base.to_string()
    } else {
        format!("{base}-{account}")
    }
}

/// The accounts that have a key stored for `base`, main one first.
pub fn accounts(base: &str) -> Vec<u8> {
    (1..=MAX_ACCOUNTS).filter(|a| present(&account_key(base, *a))).collect()
}

tokio::task_local! {
    /// Which account the requests made inside this task use.
    static ACCOUNT: u8;
}

/// Runs `work` with every API request in it made on `account`'s key.
pub async fn on_account<F: std::future::Future>(account: u8, work: F) -> F::Output {
    ACCOUNT.scope(account, work).await
}

/// The key for `base` on the account the current task uses (the main one by default).
pub fn get_current(base: &str) -> Option<String> {
    let account = ACCOUNT.try_with(|a| *a).unwrap_or(1);
    get(&account_key(base, account))
}

fn entry(key: &str) -> Option<Entry> {
    if !known(key) {
        return None;
    }
    Entry::new(SERVICE, key).ok()
}

pub fn get(key: &str) -> Option<String> {
    entry(key)?.get_password().ok().filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    if value.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(value).map_err(|e| e.to_string())
}

pub fn clear(key: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn present(key: &str) -> bool {
    get(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_account_keys_are_known_only_in_range() {
        assert!(known("gemini-api-key"));
        assert!(known("gemini-api-key-2"));
        assert!(known("openai-api-key-5"));
        assert!(!known("gemini-api-key-6"));
        assert!(!known("gemini-api-key-1"));
        assert!(!known("github-token-2"));
        assert!(!known("gemini-api-key-x"));
        assert_eq!(account_key("gemini-api-key", 1), "gemini-api-key");
        assert_eq!(account_key("gemini-api-key", 3), "gemini-api-key-3");
    }
}
