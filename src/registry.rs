use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Registry {
    base: PathBuf,
}

impl Registry {
    pub fn from_env() -> Self {
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("index-repo"),
            _ => {
                let uid = unsafe { libc::getuid() };
                PathBuf::from(format!("/tmp/index-repo-{uid}"))
            }
        };
        Self { base }
    }

    pub fn with_base(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    pub fn roots_dir(&self) -> PathBuf {
        self.base.join("roots")
    }

    fn serve_lock_path(&self) -> PathBuf {
        self.base.join("serve.lock")
    }

    pub fn canonical(path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
    }

    pub fn hash(canonical: &Path) -> String {
        let digest = Sha1::digest(canonical.as_os_str().as_encoded_bytes());
        let hex = hex_encode(&digest);
        hex[..16].to_string()
    }

    pub fn register(&self, path: &Path, pid: u32) -> Result<()> {
        let canonical = Self::canonical(path)?;
        let roots = self.roots_dir();
        std::fs::create_dir_all(&roots)
            .with_context(|| format!("create_dir_all {}", roots.display()))?;
        let marker = roots.join(format!("{}.{}", Self::hash(&canonical), pid));
        std::fs::write(&marker, canonical.to_string_lossy().as_bytes())
            .with_context(|| format!("write marker {}", marker.display()))?;
        Ok(())
    }

    pub fn unregister(&self, path: &Path, pid: u32) -> Result<()> {
        let canonical = Self::canonical(path)?;
        let marker = self
            .roots_dir()
            .join(format!("{}.{}", Self::hash(&canonical), pid));
        match std::fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove marker {}", marker.display())),
        }
    }

    pub fn scan(&self) -> Result<Vec<PathBuf>> {
        let roots = self.roots_dir();
        let entries = match std::fs::read_dir(&roots) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("read_dir {}", roots.display())),
        };

        // BTreeMap dedupes by path and yields deterministic ordering.
        let mut live: BTreeMap<PathBuf, ()> = BTreeMap::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some((_, pid_str)) = name.rsplit_once('.') else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };

            let path = entry.path();
            if pid_alive(pid) {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    live.insert(PathBuf::from(contents), ());
                }
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }

        Ok(live.into_keys().collect())
    }

    /// Caller MUST keep the returned `File` alive to hold the lock — `flock`
    /// is released when the file descriptor is dropped.
    pub fn acquire_serve_lock(&self) -> Result<Option<std::fs::File>> {
        use std::os::unix::io::AsRawFd;

        std::fs::create_dir_all(&self.base)
            .with_context(|| format!("create_dir_all {}", self.base.display()))?;
        let path = self.serve_lock_path();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;

        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            return Ok(Some(file));
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        match errno {
            Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN => Ok(None),
            _ => Err(std::io::Error::last_os_error())
                .with_context(|| format!("flock {}", path.display())),
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(e) if e == libc::EPERM
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn register_then_scan_returns_root() {
        let base = tempdir().unwrap();
        let root = tempdir().unwrap();
        let reg = Registry::with_base(base.path());

        let pid = std::process::id();
        reg.register(root.path(), pid).unwrap();

        let canonical = Registry::canonical(root.path()).unwrap();
        let scanned = reg.scan().unwrap();
        assert_eq!(scanned, vec![canonical]);
    }

    #[test]
    fn unregister_removes_marker() {
        let base = tempdir().unwrap();
        let root = tempdir().unwrap();
        let reg = Registry::with_base(base.path());

        let pid = std::process::id();
        reg.register(root.path(), pid).unwrap();
        reg.unregister(root.path(), pid).unwrap();

        assert!(reg.scan().unwrap().is_empty());
    }

    #[test]
    fn dead_pid_is_gc_d() {
        let base = tempdir().unwrap();
        let root = tempdir().unwrap();
        let reg = Registry::with_base(base.path());

        // Obtain a guaranteed-dead pid: fork a child that immediately exits,
        // then waitpid() to fully reap it. After reaping, the pid no longer
        // refers to any process (until pid recycling), so kill(pid, 0) -> ESRCH.
        let dead_pid = unsafe {
            let child = libc::fork();
            assert!(child >= 0, "fork failed");
            if child == 0 {
                libc::_exit(0);
            }
            let mut status: libc::c_int = 0;
            libc::waitpid(child, &mut status, 0);
            child as u32
        };
        assert!(!pid_alive(dead_pid), "expected reaped child to be dead");

        // Manually create the marker for the dead pid.
        let canonical = Registry::canonical(root.path()).unwrap();
        let roots = reg.roots_dir();
        std::fs::create_dir_all(&roots).unwrap();
        let marker = roots.join(format!("{}.{}", Registry::hash(&canonical), dead_pid));
        std::fs::write(&marker, canonical.to_string_lossy().as_bytes()).unwrap();

        let scanned = reg.scan().unwrap();
        assert!(scanned.is_empty(), "dead pid root should be omitted");
        assert!(!marker.exists(), "dead pid marker should be GC'd");
    }

    #[test]
    fn same_root_two_pids_dedupes() {
        let base = tempdir().unwrap();
        let root = tempdir().unwrap();
        let reg = Registry::with_base(base.path());

        let pid_a = std::process::id();
        let pid_b = unsafe { libc::getppid() } as u32;
        assert_ne!(pid_a, pid_b);
        assert!(pid_alive(pid_b), "parent process should be alive");

        reg.register(root.path(), pid_a).unwrap();
        reg.register(root.path(), pid_b).unwrap();

        let canonical = Registry::canonical(root.path()).unwrap();
        let scanned = reg.scan().unwrap();
        assert_eq!(scanned, vec![canonical.clone()]);

        // Both live markers must remain on disk.
        let roots = reg.roots_dir();
        let marker_a = roots.join(format!("{}.{}", Registry::hash(&canonical), pid_a));
        let marker_b = roots.join(format!("{}.{}", Registry::hash(&canonical), pid_b));
        assert!(marker_a.exists());
        assert!(marker_b.exists());
    }

    #[test]
    fn serve_lock_is_exclusive() {
        let base = tempdir().unwrap();
        let reg = Registry::with_base(base.path());

        let first = reg.acquire_serve_lock().unwrap();
        assert!(first.is_some(), "first acquire should succeed");

        let reg2 = Registry::with_base(base.path());
        let second = reg2.acquire_serve_lock().unwrap();
        assert!(second.is_none(), "second acquire should be blocked");

        drop(first);
    }
}
