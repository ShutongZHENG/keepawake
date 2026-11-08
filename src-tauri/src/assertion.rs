//! IOKit power-assertion FFI and the RAII [`AssertionGuard`] handle.

use std::ffi::c_void;
use std::ptr;

/// Opaque CoreFoundation string pointer (`CFStringRef`).
type CFStringRef = *const c_void;
/// CoreFoundation generic object pointer (`CFTypeRef`).
type CFTypeRef = *const c_void;
/// `IOPMAssertionId` — kernel-assigned assertion identifier.
type IOPMAssertionId = u32;
/// `kern_return_t`.
type IOReturn = i32;

/// `kCFStringEncodingUTF8`
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kIOPMAssertionLevelOn` — full strength (255).
const IOPM_ASSERTION_LEVEL_ON: u32 = 255;
/// `kERN_SUCCESS`
const IOR_SUCCESS: IOReturn = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut IOPMAssertionId,
    ) -> IOReturn;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionId) -> IOReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(
        alloc: *const c_void,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> CFStringRef;
    fn CFRelease(cf: CFTypeRef);
}

/// RAII handle for a power assertion: released in [`Drop`].
pub struct AssertionGuard(u32);

impl Drop for AssertionGuard {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

/// Create an owned `CFStringRef` from UTF-8 bytes (released by the caller).
fn cf_string(text: &str) -> Result<CFStringRef, String> {
    let ptr = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            text.as_ptr(),
            text.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    if ptr.is_null() {
        Err(format!("CFStringCreateWithBytes failed for {text:?}"))
    } else {
        Ok(ptr)
    }
}

/// Create an assertion of the given IOKit type name; the guard releases it.
fn create_assertion(type_name: &str) -> Result<AssertionGuard, String> {
    let assertion_type = cf_string(type_name)?;
    let assertion_name = match cf_string("KeepAwake") {
        Ok(name) => name,
        Err(err) => {
            unsafe { CFRelease(assertion_type as CFTypeRef) };
            return Err(err);
        }
    };
    let mut assertion_id: IOPMAssertionId = 0;
    let status = unsafe {
        IOPMAssertionCreateWithName(
            assertion_type,
            IOPM_ASSERTION_LEVEL_ON,
            assertion_name,
            &mut assertion_id,
        )
    };
    unsafe {
        CFRelease(assertion_type as CFTypeRef);
        CFRelease(assertion_name as CFTypeRef);
    }
    if status == IOR_SUCCESS {
        Ok(AssertionGuard(assertion_id))
    } else {
        Err(format!(
            "IOPMAssertionCreateWithName({type_name}) failed with {status}"
        ))
    }
}

/// Hold `kIOPMAssertionTypePreventUserIdleSystemSleep` until the guard drops.
pub fn assert_prevent_system_sleep() -> Result<AssertionGuard, String> {
    create_assertion("PreventUserIdleSystemSleep")
}

/// Hold `kIOPMAssertionTypePreventUserIdleDisplaySleep` until the guard drops.
pub fn assert_prevent_display_sleep() -> Result<AssertionGuard, String> {
    create_assertion("PreventUserIdleDisplaySleep")
}

#[cfg(test)]
impl AssertionGuard {
    /// Test-only guard that never created a real assertion. Its Drop releases
    /// id `0`, which IOKit rejects as a no-op error (no real state touched).
    pub fn fake() -> Self {
        Self(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the IOKit symbols link: creates a real assertion and releases it
    /// via RAII. Ignored by default so routine `cargo test` runs never touch
    /// real power state; run once manually with `cargo test -- --ignored`.
    #[test]
    #[ignore = "creates a real IOKit assertion; run once manually"]
    fn system_sleep_assertion_links_and_releases() {
        let guard = assert_prevent_system_sleep()
            .expect("IOKit FFI must link and create a PreventUserIdleSystemSleep assertion");
        drop(guard); // Drop releases the assertion
    }
}
