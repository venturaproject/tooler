use anyhow::{Context, Result};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// An advisory exclusive lock kept alive by its open file handle. Unlike a sentinel file,
/// this is released by the OS if its owner exits unexpectedly and never needs stale-file
/// deletion.
pub struct FileLock {
    _file: File,
}

#[cfg(unix)]
fn lock_file(file: &File, blocking: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    const LOCK_EX: std::os::raw::c_int = 2;
    const LOCK_NB: std::os::raw::c_int = 4;
    let operation = if blocking { LOCK_EX } else { LOCK_EX | LOCK_NB };
    if unsafe { flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn lock_file(file: &File, blocking: bool) -> std::io::Result<()> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            length_low: u32,
            length_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 1;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 2;
    let flags = LOCKFILE_EXCLUSIVE_LOCK
        | if blocking {
            0
        } else {
            LOCKFILE_FAIL_IMMEDIATELY
        };
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: ptr::null_mut(),
    };
    if unsafe {
        LockFileEx(
            file.as_raw_handle().cast(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } != 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_file(_: &File, _: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file locking is not supported on this platform",
    ))
}

pub fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

pub fn lock(path: &Path) -> Result<FileLock> {
    let lock_path = lock_path(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock {}", lock_path.display()))?;
    lock_file(&file, true).with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(FileLock { _file: file })
}

pub fn try_lock(path: &Path) -> Result<Option<FileLock>> {
    let lock_path = lock_path(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock {}", lock_path.display()))?;
    match lock_file(&file, false) {
        Ok(()) => Ok(Some(FileLock { _file: file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error).with_context(|| format!("locking {}", lock_path.display())),
    }
}

/// Replaces a file only after its complete contents have been durably written. The temporary
/// file lives beside the destination so `persist` is an atomic rename on supported platforms.
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".tooler-")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary file beside {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary
        .write_all(contents)
        .with_context(|| format!("writing temporary file for {}", path.display()))?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    // A durable file name is needed too: without syncing the directory, a power loss can
    // lose the completed rename even though the temporary file itself was synced.
    #[cfg(unix)]
    File::open(parent)
        .with_context(|| format!("opening parent directory for {}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing parent directory for {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_file_with_complete_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, b"old").unwrap();
        write(&path, b"new complete contents").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new complete contents");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn file_lock_is_released_when_guard_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let guard = try_lock(&path).unwrap().unwrap();
        assert!(try_lock(&path).unwrap().is_none());
        drop(guard);
        assert!(try_lock(&path).unwrap().is_some());
    }
}
