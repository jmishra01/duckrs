//! A temporary directory for the files a test process writes: made on first use, removed when the
//! process exits, and, when an earlier process was killed before it could remove its own, removed
//! the next time one starts.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

/// `{temp}/{prefix}{pid}`, the same directory for every test in this process.
pub fn directory(prefix: &str) -> &'static Path {
    DIRECTORY.get_or_init(|| {
        sweep(prefix);
        let directory = std::env::temp_dir().join(format!("{prefix}{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        // The test harness ends with `exit`, which runs this; statics are never dropped.
        #[cfg(unix)]
        // SAFETY: `remove` is a plain function that touches nothing but the directory above.
        unsafe {
            libc::atexit(remove);
        }
        directory
    })
}

extern "C" fn remove() {
    if let Some(directory) = DIRECTORY.get() {
        let _ = std::fs::remove_dir_all(directory);
    }
}

/// Removes the `{prefix}{pid}` directories whose process is gone.
fn sweep(prefix: &str) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(|pid| pid.parse().ok());
        if pid.is_some_and(|pid| !alive(pid)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn alive(_pid: i32) -> bool {
    true
}
