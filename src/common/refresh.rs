// --refresh: what a build fetched or locked before counts as old, newer builds and files are taken
use std::sync::OnceLock;
use std::time::SystemTime;

/// When --refresh was given, None without
pub static REFRESHED_AT: OnceLock<SystemTime> = OnceLock::new();

/// From now on, whatever was fetched before counts as expired
pub fn refresh() {
    let _ = REFRESHED_AT.set(SystemTime::now());
}
