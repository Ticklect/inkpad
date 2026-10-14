use std::path::Path;

#[cfg(windows)]
pub struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl InstanceGuard {
    /// An OS mutex prevents two app windows overwriting the same saved notes.
    /// Data folders get distinct mutexes so portable/isolated profiles work.
    pub fn acquire(data_file: &Path) -> Result<Option<Self>, String> {
        use std::hash::{Hash, Hasher};
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        data_file.to_string_lossy().to_lowercase().hash(&mut hash);
        let name = format!("Local\\InkpadWorkspace-{:016x}", hash.finish());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the UTF-16 buffer remains alive for the call; the owned
        // handle is closed in Drop. No raw pointer is retained by Rust.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
            if handle.is_null() {
                return Err(format!("Could not lock the notebook: {}", std::io::Error::last_os_error()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Ok(None);
            }
            Ok(Some(Self(handle)))
        }
    }
}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: valid mutex handle from CreateMutexW, owned by this guard.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); }
    }
}

#[cfg(not(windows))]
pub struct InstanceGuard;

#[cfg(not(windows))]
impl InstanceGuard {
    pub fn acquire(_data_file: &Path) -> Result<Option<Self>, String> { Ok(Some(Self)) }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn protects_same_file_but_allows_other_profiles() {
        let root = std::env::temp_dir().join(format!("inkpad-lock-{}", std::process::id()));
        let first = InstanceGuard::acquire(&root.join("a.json")).unwrap().unwrap();
        assert!(InstanceGuard::acquire(&root.join("a.json")).unwrap().is_none());
        assert!(InstanceGuard::acquire(&root.join("b.json")).unwrap().is_some());
        drop(first);
        assert!(InstanceGuard::acquire(&root.join("a.json")).unwrap().is_some());
    }
}
