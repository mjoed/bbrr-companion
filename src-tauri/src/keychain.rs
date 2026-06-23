//! access-token storage in the OS keychain (Windows Credential Manager, macOS
//! Keychain, Secret Service on Linux). the token is never written to disk in
//! plain text or to logs.

use keyring::Entry;

const SERVICE: &str = "gg.raidreview.companion";
const ACCOUNT: &str = "access-token";

fn entry() -> Result<Entry, String> {
    Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())
}

pub fn store_token(token: &str) -> Result<(), String> {
    entry()?.set_password(token).map_err(|e| e.to_string())
}

pub fn get_token() -> Option<String> {
    let entry = match entry() {
        Ok(e) => e,
        Err(err) => {
            eprintln!("keychain unavailable: {err}");
            return None;
        }
    };
    match entry.get_password() {
        Ok(token) => Some(token),
        // genuinely no stored token — the normal "signed out" case.
        Err(keyring::Error::NoEntry) => None,
        // a real backend failure (locked Secret Service, denied prompt, missing
        // keyring): don't silently mask it as a logout — leave a trace. still
        // return None so the app falls back to the sign-in screen.
        Err(err) => {
            eprintln!("keychain read failed: {err}");
            None
        }
    }
}

pub fn clear_token() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
