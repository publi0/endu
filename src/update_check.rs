//! Detects a newer app bundle on disk and relaunches into it.
//!
//! Homebrew replaces `/Applications/Endu.app` while the running process keeps
//! executing the old binary. Comparing the on-disk bundle version with the
//! compiled version lets the window offer a "Restart to update" action. A
//! detached helper waits for this process to exit and only then reopens the
//! bundle, so the new process never meets the old instance or its lock.

use std::path::{Path, PathBuf};

use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSString};

/// The installed bundle path when running from an app bundle, or `None` for
/// ad hoc binaries (cargo run, tests).
pub fn bundle_path() -> Option<PathBuf> {
    bundle_from_executable(&std::env::current_exe().ok()?)
}

/// Walks up from the executable to the enclosing .app bundle. The binary must
/// sit in the bundle's Contents directory; a directory that merely sits next
/// to an .app does not count.
fn bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    let bundle = executable.ancestors().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".app"))
    })?;
    executable
        .ancestors()
        .nth(2)
        .is_some_and(|contents| contents.file_name().is_some_and(|name| name == "Contents"))
        .then(|| bundle.to_path_buf())
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
    let dictionary =
        unsafe { NSDictionary::<NSString, AnyObject>::dictionaryWithContentsOfFile(&file) }?;
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

/// Seconds the helper waits for this process to exit before reopening anyway.
const RELAUNCH_WAIT_TENTHS: u32 = 300;

/// A detached shell that waits for `pid` to exit, then runs `opener` on
/// `bundle`. Arguments are positional, never interpolated into the script.
fn relaunch_command(pid: u32, bundle: &Path, opener: &str) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let script = format!(
        "pid=$1; app=$2; i=0\n\
         while kill -0 \"$pid\" 2>/dev/null && [ \"$i\" -lt {RELAUNCH_WAIT_TENTHS} ]; do sleep 0.1; i=$((i+1)); done\n\
         exec \"$3\" \"$app\""
    );
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .arg("hex-relaunch")
        .arg(pid.to_string())
        .arg(bundle)
        .arg(opener)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Its own process group: quitting Hex must not take the helper with it.
        .process_group(0);
    command
}

/// Reopens the app bundle once this process has exited, then quits. Opening
/// it while this instance still runs would only reactivate the old process.
/// Returns `false` when the helper could not start, so the UI can say so.
pub fn relaunch_and_quit(bundle: &Path) -> bool {
    match relaunch_command(std::process::id(), bundle, "/usr/bin/open").spawn() {
        Ok(child) => {
            tracing::info!(helper = child.id(), "relaunch scheduled after exit");
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
    fn relaunch_waits_for_the_old_process_to_exit() {
        let folder = std::env::temp_dir().join(format!("hex-relaunch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let marker = folder.join("reopened");
        let mut old = std::process::Command::new("/bin/sleep")
            .arg("0.6")
            .spawn()
            .unwrap();
        let mut helper = relaunch_command(old.id(), &marker, "/usr/bin/touch")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!marker.exists(), "reopened while the old process was alive");
        old.wait().unwrap();
        assert!(helper.wait().unwrap().success());
        assert!(marker.exists());
        // A path with spaces and shell syntax stays one literal argument.
        let tricky = folder.join("Hex $(touch pwned) ; x.app");
        assert!(
            relaunch_command(u32::MAX, &tricky, "/usr/bin/touch")
                .status()
                .unwrap()
                .success()
        );
        assert!(tricky.exists());
        assert!(!folder.join("pwned").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

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

    #[test]
    fn bundle_paths_resolve_from_the_executable_location() {
        let app = Path::new("/Applications/Endu.app");
        let bundled = app.join("Contents/MacOS/endu");
        assert_eq!(bundle_from_executable(&bundled).as_deref(), Some(app));
        // A binary outside a bundle's Contents directory never counts.
        assert_eq!(bundle_from_executable(Path::new("/tmp/hex")), None);
        assert_eq!(
            bundle_from_executable(Path::new("/tmp/Endu.app/Contents")),
            None
        );
        // A directory named like a bundle next to the executable is not enough.
        assert_eq!(
            bundle_from_executable(Path::new("/tmp/Endu.app/other/endu")),
            None
        );
    }
}
