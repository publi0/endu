//! Detects a newer app bundle on disk and relaunches into it.
//!
//! Homebrew replaces `/Applications/Hex.app` while the running process keeps
//! executing the old binary. Comparing the on-disk bundle version with the
//! compiled version lets the window offer a "Restart to update" action; the
//! relaunch is scheduled with `open` just before quitting so the instance
//! lock is released first.

use std::path::PathBuf;

use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSString};

/// The installed bundle path when running from an app bundle, or `None` for
/// ad hoc binaries (cargo run, tests).
pub fn bundle_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let mut path = executable.ancestors().nth(3)?;
    if path.file_name()? != "Contents" {
        return None;
    }
    path = path.parent()?;
    path.file_name().and_then(|name| name.to_str())?.ends_with(".app")
        .then(|| path.to_path_buf())
}

/// The on-disk bundle's marketing version, if it is newer than the running
/// binary's compiled version.
pub fn pending_update() -> Option<String> {
    let bundle = bundle_path()?;
    let path = bundle.join("Contents/Info.plist");
    let file = NSString::from_str(&path.to_string_lossy());
    // The binding marks this initializer deprecated without offering a
    // replacement; it remains the direct way to read a property list.
    #[allow(deprecated)]
    let dictionary = unsafe {
        NSDictionary::<NSString, AnyObject>::dictionaryWithContentsOfFile(&file)
    }?;
    let key = NSString::from_str("CFBundleShortVersionString");
    let version = dictionary
        .objectForKey(&key)?
        .downcast::<NSString>()
        .ok()?
        .to_string();
    (version_newer(&version, env!("CARGO_PKG_VERSION"))).then_some(version)
}

fn version_newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

/// Schedules the app bundle to reopen right after this process exits, then
/// quits. `open` waits for the bundle to become available, so the instance
/// lock is released before the new process starts. Returns `false` when the
/// relaunch could not be scheduled so the UI can surface the failure.
pub fn relaunch_and_quit(bundle: &PathBuf) -> bool {
    let result = std::process::Command::new("/usr/bin/open")
        .arg("-a")
        .arg(bundle)
        .spawn();
    match result {
        Ok(child) => {
            tracing::info!(pid = child.id(), "relaunch scheduled");
            crate::desktop::request_quit();
            true
        }
        Err(error) => {
            tracing::error!(%error, "could not schedule the relaunch");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_versions_compare_component_wise() {
        assert!(version_newer("3.2.0", "3.1.3"));
        assert!(version_newer("3.1.4", "3.1.3"));
        assert!(!version_newer("3.1.3", "3.1.3"));
        assert!(!version_newer("3.1.2", "3.1.3"));
        assert!(!version_newer("2.9.9", "3.1.3"));
        assert!(version_newer("4.0", "3.1.3"));
    }

    #[test]
    fn ad_hoc_binaries_have_no_bundle() {
        // Tests run from target/debug/deps, not an app bundle.
        assert!(bundle_path().is_none());
        assert!(pending_update().is_none());
    }
}
