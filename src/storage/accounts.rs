//! Reentrant UID lookup; copy the account name before releasing NSS storage.
use super::{Result, invalid};
use std::ffi::CStr;

pub(super) fn name(uid: u32, buffer_size: usize, absent: &str, malformed: &str) -> Result<String> {
    let mut buffer = vec![0u8; buffer_size];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: the writable record and buffer remain live through lookup and
    // copying. getpwuid_r supplies pw_name inside that buffer on success.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if code != 0 {
        return Err(std::io::Error::from_raw_os_error(code).into());
    }
    if found.is_null() {
        return Err(invalid(absent));
    }
    // SAFETY: successful NSS lookup supplies a NUL-terminated account name.
    Ok(unsafe { CStr::from_ptr(entry.pw_name) }
        .to_str()
        .map_err(|_| invalid(malformed))?
        .to_owned())
}
