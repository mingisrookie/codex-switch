//! Preserve AppKit's native quit protocol while applying the mutation exit gate.
//!
//! The locked Tao delegate handles applicationWillTerminate but does not
//! implement applicationShouldTerminate. Add only that missing method to its
//! exact class; keep its existing delegate instance and termination callback.

use std::{
    ffi::{c_char, c_void, CStr},
    panic::{catch_unwind, UnwindSafe},
    sync::atomic::{AtomicBool, Ordering},
};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

const TERMINATE_CANCEL: usize = 0;
const TERMINATE_NOW: usize = 1;
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

// Both supported Mac targets use the 64-bit Objective-C ABI. NSUInteger is an
// unsigned 64-bit integer (Q); self and sender are objects (@), _cmd a selector (:).
type TerminationMethod = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> usize;

#[link(name = "objc")]
unsafe extern "C" {
    fn object_getClass(object: *mut c_void) -> *mut c_void;
    fn class_getName(class: *mut c_void) -> *const c_char;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn class_getInstanceMethod(class: *mut c_void, selector: *mut c_void) -> *mut c_void;
    // Objective-C BOOL occupies one byte on both supported Mac architectures.
    fn class_addMethod(
        class: *mut c_void,
        selector: *mut c_void,
        implementation: TerminationMethod,
        types: *const c_char,
    ) -> i8;
}

pub(crate) fn install() -> Result<(), &'static str> {
    if usize::BITS != 64 {
        return Err("native quit hook requires the 64-bit macOS ABI");
    }
    let mtm = MainThreadMarker::new().ok_or("native quit hook requires the main thread")?;
    if HOOK_INSTALLED.load(Ordering::Acquire) {
        return Err("native quit hook is already installed");
    }
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app
        .delegate()
        .ok_or("native quit hook requires a delegate")?;
    let delegate_ptr = std::ptr::from_ref(&*delegate).cast_mut().cast::<c_void>();
    // These are live AppKit objects retained above. No untrusted pointer is
    // dereferenced, and the class must be exactly the locked Tao implementation.
    let class = unsafe { object_getClass(delegate_ptr) };
    if class.is_null() {
        return Err("native quit delegate class is unavailable");
    }
    let name = unsafe { class_getName(class) };
    if name.is_null() || unsafe { CStr::from_ptr(name) } != c"TaoAppDelegateParent" {
        return Err("native quit delegate class is not recognized");
    }
    let selector = unsafe { sel_registerName(c"applicationShouldTerminate:".as_ptr()) };
    if selector.is_null() || !unsafe { class_getInstanceMethod(class, selector) }.is_null() {
        return Err("native quit delegate already has a termination decision");
    }
    // class_addMethod never replaces an existing implementation. The encoding
    // exactly matches TerminationMethod on the two 64-bit Mac targets.
    if unsafe {
        class_addMethod(
            class,
            selector,
            application_should_terminate,
            c"Q@:@".as_ptr(),
        )
    } == 0
    {
        return Err("native quit hook could not be installed");
    }
    // AppKit can cache optional delegate selectors when its delegate is set.
    // Re-register this same retained object before Ready/the event loop; Tao's
    // ownership, launch/open-url callbacks and WillTerminate method are kept.
    app.setDelegate(None);
    app.setDelegate(Some(&delegate));
    HOOK_INSTALLED.store(true, Ordering::Release);
    Ok(())
}

extern "C" fn application_should_terminate(
    this: *mut c_void,
    _selector: *mut c_void,
    sender: *mut c_void,
) -> usize {
    // Catch Rust unwinding before it reaches the Objective-C ABI. Release builds
    // use panic=abort; this does not claim to recover an abort or NSException.
    termination_reply(
        || {
            if !HOOK_INSTALLED.load(Ordering::Acquire) || this.is_null() || sender.is_null() {
                return false;
            }
            let Some(mtm) = MainThreadMarker::new() else {
                return false;
            };
            let app = NSApplication::sharedApplication(mtm);
            if sender != std::ptr::from_ref(&*app).cast_mut().cast::<c_void>() {
                return false;
            }
            let Some(delegate) = app.delegate() else {
                return false;
            };
            if this != std::ptr::from_ref(&*delegate).cast_mut().cast::<c_void>() {
                return false;
            }

            crate::commands::reserve_native_app_exit().unwrap_or(false)
        },
        |prevented| {
            if let Some(runtime) = crate::diagnostics::global_runtime() {
                let _ = runtime
                    .lifecycle()
                    .record_exit_requested("nativeQuit", prevented);
            }
        },
    )
}

fn termination_reply(
    decide: impl FnOnce() -> bool + UnwindSafe,
    record: impl FnOnce(bool) + UnwindSafe,
) -> usize {
    let allowed = catch_unwind(decide).unwrap_or(false);
    // Recording is best effort. Once the mutation reservation is safely held,
    // a logging panic must not cancel the approved quit and strand that lock.
    let _ = catch_unwind(|| record(!allowed));
    if allowed {
        TERMINATE_NOW
    } else {
        TERMINATE_CANCEL
    }
}

#[cfg(test)]
mod tests {
    use super::{termination_reply, TERMINATE_CANCEL, TERMINATE_NOW};

    #[test]
    fn macos_native_quit_reply_preserves_allow_and_cancel() {
        assert_eq!(termination_reply(|| true, |_| {}), TERMINATE_NOW);
        assert_eq!(termination_reply(|| false, |_| {}), TERMINATE_CANCEL);
    }

    #[test]
    fn macos_native_quit_panic_cannot_unwind_into_appkit() {
        assert_eq!(
            termination_reply(|| panic!("isolated native quit fixture"), |_| {}),
            TERMINATE_CANCEL
        );
    }

    #[test]
    fn macos_native_quit_logging_panic_preserves_the_reserved_decision() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let reserved = AtomicBool::new(false);
        let reply = termination_reply(
            || {
                reserved.store(true, Ordering::Release);
                true
            },
            |_| panic!("isolated diagnostic failure fixture"),
        );
        assert!(reserved.load(Ordering::Acquire));
        assert_eq!(reply, TERMINATE_NOW);
    }
}
