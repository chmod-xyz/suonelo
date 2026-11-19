use crate::error::{AppError, Result};
use objc2::{
    MainThreadMarker,
    runtime::{AnyClass, AnyObject, Imp, Sel},
    sel,
};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use std::sync::OnceLock;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

static APPLICATION: OnceLock<AppHandle> = OnceLock::new();

pub fn install_quit_handler(handle: &AppHandle) -> Result<()> {
    let unavailable = || {
        AppError::new(
            "macos_quit_handler",
            "startup",
            "suonelo could not install its macOS quit handler.",
            true,
        )
    };
    let main_thread = MainThreadMarker::new().ok_or_else(unavailable)?;
    let application = NSApplication::sharedApplication(main_thread);
    let delegate = application.delegate().ok_or_else(unavailable)?;
    let delegate_object: &AnyObject = (*delegate).as_ref();
    let delegate_class = delegate_object.class();
    let selector = sel!(applicationShouldTerminate:);
    // Tao delivers Cocoa termination only after it is committed. Add the optional
    // AppKit delegate callback without replacing any of Tao's existing methods.
    // The implementation's ABI is NSUInteger return, self, selector, NSApplication.
    let installed = unsafe {
        if !objc2::ffi::class_getInstanceMethod(delegate_class, selector).is_null() {
            return Err(unavailable());
        }
        let implementation = std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, &NSApplication) -> NSApplicationTerminateReply,
            Imp,
        >(application_should_terminate);
        objc2::ffi::class_addMethod(
            std::ptr::from_ref::<AnyClass>(delegate_class).cast_mut(),
            selector,
            implementation,
            c"Q@:@".as_ptr(),
        )
        .as_bool()
    };
    if !installed {
        return Err(unavailable());
    }
    let _ = APPLICATION.set(handle.clone());
    // Refresh AppKit's delegate registration after adding the optional method.
    application.setDelegate(None);
    application.setDelegate(Some(&delegate));
    Ok(())
}

extern "C-unwind" fn application_should_terminate(
    _: &AnyObject,
    _: Sel,
    _: &NSApplication,
) -> NSApplicationTerminateReply {
    if let Some(window) = APPLICATION.get().and_then(|app| app.get_webview_window("main"))
        && window.close().is_ok()
    {
        // Window close uses the same frontend confirmation and tracked cleanup as
        // the traffic light. The Rust shutdown command exits the Tauri event loop.
        NSApplicationTerminateReply::TerminateCancel
    } else {
        NSApplicationTerminateReply::TerminateNow
    }
}

pub fn show_startup_error(handle: &AppHandle, error: AppError) {
    eprintln!("suonelo startup [{}]: {}", error.code, error.message);
    let exit = handle.clone();
    handle
        .dialog()
        .message(error.message)
        .title("suonelo")
        .kind(MessageDialogKind::Error)
        .show(move |_| exit.exit(1));
}

pub fn webview_error() -> AppError {
    AppError::new(
        "wkwebview_startup",
        "startup",
        "suonelo could not open its window using the macOS system webview.",
        true,
    )
}
