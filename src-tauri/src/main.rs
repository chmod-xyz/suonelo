#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod commands;
mod error;
mod link_import;
mod locations;
#[cfg(target_os = "macos")]
mod macos;
mod media;
#[cfg(all(windows, feature = "preload-prototype"))]
mod preload;
#[cfg(any(not(target_os = "macos"), test))]
mod profile_migration;
mod qobuz;
mod queue;
mod recovery;
mod release_folder;
mod search;
mod storage;
mod vault;

#[cfg(any(target_os = "macos", all(windows, feature = "preload-prototype")))]
use tauri::Manager;
use tauri::{Emitter, WebviewWindowBuilder, WindowEvent};

fn main() {
    #[cfg(all(windows, feature = "preload-prototype"))]
    if preload::begin().is_err() {
        return;
    }
    if !check_webview_runtime() {
        show_error(
            "suonelo requires the Microsoft Edge WebView2 Evergreen Runtime. Install it from https://developer.microsoft.com/microsoft-edge/webview2/ and start suonelo again.",
        );
        return;
    }
    #[cfg(not(target_os = "macos"))]
    let state = match app::App::initialize() {
        Ok(state) => state,
        Err(error) => {
            show_error(&error.message);
            return;
        }
    };
    #[cfg(all(windows, feature = "preload-prototype"))]
    preload::mark("profile_lock_acquired_storage_ready", serde_json::json!({}));
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init());
    #[cfg(not(target_os = "macos"))]
    let builder = builder.manage(state.clone());
    let builder = builder
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if let Err(error) = setup_macos(app) {
                macos::show_startup_error(app.handle(), error);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = state.handle.set(app.handle().clone());
                let window = app
                    .config()
                    .app
                    .windows
                    .first()
                    .ok_or("Missing main window configuration")?;
                #[cfg(windows)]
                let window = &{
                    let mut config = window.clone();
                    config.title = format!("suonelo v{}", env!("CARGO_PKG_VERSION"));
                    config
                };
                #[cfg(all(windows, feature = "preload-prototype"))]
                preload::build(app, window, &state.root)?;
                #[cfg(not(all(windows, feature = "preload-prototype")))]
                WebviewWindowBuilder::from_config(app, window)?
                    .data_directory(state.root.join("data").join("webview"))
                    .build()?;
                tauri::async_runtime::spawn(state.clone().start());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(all(windows, feature = "preload-prototype"))]
                if matches!(window.label(), "main" | "startup")
                    && let Some(exit) = preload::native_close(window)
                {
                    api.prevent_close();
                    if exit {
                        window.app_handle().exit(0);
                    }
                    return;
                }
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.emit("close-requested", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::preload_frontend_ready,
            commands::preload_test_close,
            commands::queue_snapshot,
            commands::set_queue_paused,
            commands::subscribe_queue,
            commands::submit_links,
            commands::inspect_link_file,
            commands::submit_imported_links,
            commands::clear_downloads,
            commands::retry_job,
            commands::retry_file,
            commands::replace_search_audio,
            commands::download_again,
            commands::cancel_job,
            commands::remove_job,
            commands::apply_settings_to_pending,
            commands::decide_large_submission,
            commands::list_bookmarks,
            commands::subscribe_bookmarks,
            commands::download_bookmarks,
            commands::retry_bookmark,
            commands::remove_bookmarks,
            commands::clear_bookmarks,
            commands::read_settings,
            commands::patch_settings,
            commands::save_connection_limit,
            commands::migrate_theme,
            commands::reset_settings,
            commands::reset_vault,
            commands::validate_credentials,
            commands::start_email_sign_in,
            commands::confirm_account_validation,
            commands::cancel_account_validation,
            commands::open_containing_folder,
            commands::release_folder_status,
            commands::open_release_folder,
            commands::retry_connection,
            commands::search_genres,
            commands::search_query,
            commands::search_cancel,
            commands::search_detail,
            commands::search_artwork,
            commands::clear_search_history,
            commands::prepare_search_submission,
            commands::commit_search_submission,
            commands::metadata_catalog,
            commands::preview_output,
            commands::request_close,
            commands::shutdown_session,
            commands::default_music_location,
            commands::open_credits,
            commands::open_about_link,
        ]);
    #[cfg(not(target_os = "macos"))]
    if builder.run(tauri::generate_context!()).is_err() {
        show_error(
            "suonelo could not open the desktop window. Check that the WebView2 Runtime is usable and the portable folder is writable.",
        );
    }
    #[cfg(target_os = "macos")]
    match builder.build(tauri::generate_context!()) {
        Ok(app) => app.run(|_, _| {}),
        Err(_) => show_error("suonelo could not initialize the macOS desktop runtime."),
    }
}

#[cfg(target_os = "macos")]
fn setup_macos(app: &mut tauri::App<tauri::Wry>) -> error::Result<()> {
    let profile = locations::macos_profile(app)?;
    let state = app::App::initialize_macos(profile.root, profile.qa_name.as_deref())?;
    let _ = state.handle.set(app.handle().clone());
    app.manage(state.clone());
    macos::install_quit_handler(app.handle())?;

    let window = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(macos::webview_error)?;
    WebviewWindowBuilder::from_config(app, window)
        .and_then(|builder| builder.incognito(profile.qa_name.is_some()).build())
        .map_err(|_| macos::webview_error())?;
    tauri::async_runtime::spawn(state.start());
    Ok(())
}

#[cfg(windows)]
fn check_webview_runtime() -> bool {
    use windows::{
        Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
            RegGetValueW,
        },
        core::w,
    };
    let subkey =
        w!(r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}");
    [
        (HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ | RRF_SUBKEY_WOW6432KEY),
        (HKEY_CURRENT_USER, RRF_RT_REG_SZ),
    ]
    .into_iter()
    .any(|(hive, flags)| {
        let mut value = [0u16; 64];
        let mut size = std::mem::size_of_val(&value) as u32;
        let result = unsafe {
            RegGetValueW(
                hive,
                subkey,
                w!("pv"),
                flags,
                None,
                Some(value.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        result.is_ok()
            && String::from_utf16_lossy(&value)
                .trim_end_matches('\0')
                .split('.')
                .any(|part| part.parse::<u32>().is_ok_and(|number| number > 0))
    })
}

#[cfg(not(windows))]
fn check_webview_runtime() -> bool {
    true
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
        core::{PCWSTR, w},
    };
    let message = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("suonelo"),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}
