//! macOS's `sysctlbyname`, for what the VM needs to know of this Mac: its
//! version (`doctor`), its memory (the VM's default size).

use std::ffi::CStr;

/// The string value of the sysctl `name` (`kern.osproductversion`).
pub(crate) fn string_named(name: &CStr) -> Option<String> {
    let mut buffer = [0_u8; 64];
    let mut len = buffer.len();
    // SAFETY: `name` is a C string; `buffer` and `len` are valid, `len` its
    // size, which the call updates to what it wrote; no new value is set.
    let done = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if done != 0 {
        return None;
    }
    let text = buffer.get(..len)?;
    let text = text.split(|&byte| byte == 0).next()?;
    String::from_utf8(text.to_vec()).ok()
}

/// The 64-bit number value of the sysctl `name` (`hw.memsize`).
pub(crate) fn u64_named(name: &CStr) -> Option<u64> {
    let mut value = 0_u64;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `name` is a C string; `value` is a valid u64 and `len` its size,
    // which the call checks against the value's (a sysctl of another size
    // fails, or says the size it wrote, checked below); no new value is set.
    let done = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (done == 0 && len == std::mem::size_of::<u64>()).then_some(value)
}
