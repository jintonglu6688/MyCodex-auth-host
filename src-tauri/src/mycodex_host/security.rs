//! Directory protection reused from the existing MyCodex platform adapter.
use std::path::Path;

const MARKER: &str = ".mycodex-auth-core";
const MARKER_CONTENT: &[u8] = b"MyCodex upstream core data directory v1\n";

pub(super) fn prepare_directory(path: &Path) -> Result<(), &'static str> {
    reject_links(path)?;
    if path.exists() {
        let owned = std::fs::read(path.join(MARKER)).ok().as_deref() == Some(MARKER_CONTENT);
        let empty = std::fs::read_dir(path)
            .map_err(|_| "invalid_data_dir")?
            .next()
            .is_none();
        if !owned && !empty {
            return Err("data_dir_not_owned");
        }
    } else {
        std::fs::create_dir_all(path).map_err(|_| "invalid_data_dir")?;
    }
    #[cfg(windows)]
    CurrentUserSecurity::new()?.protect_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "directory_security_failed")?;
    }
    if !path.join(MARKER).exists() {
        std::fs::write(path.join(MARKER), MARKER_CONTENT)
            .map_err(|_| "directory_security_failed")?;
    }
    Ok(())
}

// The private store may contain backups as well as the main database. Reject
// existing redirects before upstream initialization can follow a writable path.
fn reject_links(path: &Path) -> Result<(), &'static str> {
    let mut directories = vec![path.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).map_err(|_| "invalid_data_dir")? {
            let entry = entry.map_err(|_| "invalid_data_dir")?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|_| "invalid_data_dir")?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("data_dir_contains_link");
                }
            }
            if metadata.file_type().is_symlink() {
                return Err("data_dir_contains_link");
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) use windows::CurrentUserSecurity;

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SetFileSecurityW, TokenUser, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(crate) struct CurrentUserSecurity {
        pub sid: String,
        descriptor: PSECURITY_DESCRIPTOR,
    }

    impl CurrentUserSecurity {
        pub fn new() -> Result<Self, &'static str> {
            // SAFETY: token handle and aligned token buffer remain valid until
            // the SID has been copied. Every allocated Win32 object is released.
            let sid = unsafe {
                let mut token = null_mut();
                if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                    return Err("identity_failed");
                }
                let mut size = 0;
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
                if size == 0 {
                    CloseHandle(token);
                    return Err("identity_failed");
                }
                let mut buffer =
                    vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
                let success = GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    size,
                    &mut size,
                );
                CloseHandle(token);
                if success == 0 {
                    return Err("identity_failed");
                }
                let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
                sid_string(user.User.Sid)?
            };
            let sddl: Vec<u16> = format!("O:{sid}D:P(A;OICI;GA;;;{sid})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = null_mut();
            // SAFETY: sddl is terminated; the resulting allocation is owned here.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                )
            } == 0
            {
                return Err("security_descriptor_failed");
            }
            Ok(Self { sid, descriptor })
        }

        pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            }
        }

        pub fn protect_directory(&self, path: &Path) -> Result<(), &'static str> {
            use std::os::windows::ffi::OsStrExt;
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // Only this dedicated directory's DACL is set; never walk parents
            // or CodexHome. New children inherit this user-only ACL.
            if unsafe {
                SetFileSecurityW(
                    name.as_ptr(),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    self.descriptor,
                )
            } == 0
            {
                return Err("directory_security_failed");
            }
            Ok(())
        }

        pub fn verify_owner(&self, handle: HANDLE) -> Result<(), &'static str> {
            let mut owner = null_mut();
            let mut descriptor = null_mut();
            // Checking the server's pipe owner prevents a different local user
            // from impersonating the backend by pre-creating its predictable name.
            let result = unsafe {
                GetSecurityInfo(
                    handle,
                    SE_KERNEL_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut descriptor,
                )
            };
            if result != 0 {
                return Err("backend_identity_mismatch");
            }
            let actual = unsafe { sid_string(owner) };
            unsafe {
                LocalFree(descriptor);
            }
            if actual?.as_str() != self.sid {
                return Err("backend_identity_mismatch");
            }
            Ok(())
        }
    }

    impl Drop for CurrentUserSecurity {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }

    unsafe fn sid_string(sid: PSID) -> Result<String, &'static str> {
        let mut text = null_mut();
        if sid == null_mut() || ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err("identity_failed");
        }
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let result = String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| "identity_failed");
        LocalFree(text.cast());
        result
    }
}
