use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "slopbar-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `path` in 8.3 short names, or None where it has none. That happens when the volume keeps no
/// short names, when a parent folder cannot be listed (C:\Windows\Temp), or when every part of the
/// path is short already. GitHub's runners give one (C:\Users\RUNNER~1), so a test that needs it
/// asks [`require_short_names`] whether it may skip.
#[cfg(windows)]
pub fn short_spelling(path: &str) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let short = std::ffi::OsString::from_wide(&buf[..n]).into_string().ok()?;
    short.contains('~').then_some(short)
}

/// For a test that found no short spelling: fails in CI, where there always is one, so the test
/// cannot pass there without testing anything; elsewhere it lets the test skip.
#[cfg(windows)]
pub fn require_short_names(path: &str) {
    assert!(std::env::var_os("CI").is_none(), "no 8.3 short spelling for {path}");
    eprintln!("skipped: no 8.3 short spelling for {path}");
}

pub fn slot(id: &str, access: &str, refresh: &str, expires_ms: i64) -> crate::accounts::Slot {
    crate::accounts::Slot {
        id: id.into(),
        provider: crate::accounts::Provider::Claude,
        email: Some("test@example.invalid".into()),
        plan: None,
        org: None,
        data: serde_json::json!({ "credentials": { "claudeAiOauth": {
            "accessToken": access, "refreshToken": refresh, "expiresAt": expires_ms,
        } } }),
    }
}
