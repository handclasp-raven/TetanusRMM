//! Security descriptors from SDDL strings, for the state directory and the
//! helper pipe.

use std::ffi::c_void;
use std::path::Path;

use windows::core::{BOOL, HSTRING};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};

/// State directory: full control for SYSTEM and Administrators only,
/// inherited by files created inside, and nothing inherited from
/// `%ProgramData%` (which would otherwise let ordinary users read it).
pub const STATE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// A self-relative security descriptor allocated by Windows.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by the OS.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sd` receives a LocalAlloc'd descriptor, freed in Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        Ok(Self(sd))
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0 .0
    }

    fn dacl(&self) -> windows::core::Result<*mut ACL> {
        let mut present = BOOL(0);
        let mut defaulted = BOOL(0);
        let mut dacl = std::ptr::null_mut();
        // SAFETY: self.0 is a valid descriptor; dacl points into it.
        unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted)? };
        Ok(dacl)
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// Replace the DACL on `path` with [`STATE_DIR_SDDL`], blocking inheritance.
pub fn restrict_to_system_and_admins(path: &Path) -> windows::core::Result<()> {
    let sd = SecurityDescriptor::from_sddl(STATE_DIR_SDDL)?;
    let dacl = sd.dacl()?;
    // SAFETY: dacl stays valid while `sd` is alive.
    unsafe {
        SetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
        .ok()
    }
}
