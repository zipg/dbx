mod commands;
mod data_dir;
mod db;
#[cfg(target_os = "macos")]
mod macos_app_delegate;
mod models;
mod window_state_guard;

use commands::connection::AppState;
use dbx_core::sql_dialect::dialect_loader::{register_core_dialects, DialectPluginLoader, DialectRegistry};
use dbx_core::sql_dialect::hot_reload::DialectHotReload;
use dbx_core::storage::{maybe_import_user_data_db, DesktopIconTheme, DesktopSettings, Storage};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use tauri::menu::Menu;
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, MenuItem, PredefinedMenuItem, Submenu};
use tauri::webview::PageLoadEvent;
use tauri::RunEvent;
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri::{Emitter, Manager};
#[cfg(target_os = "macos")]
use tauri_plugin_clipboard_manager::ClipboardExt;
#[cfg(any(windows, target_os = "linux"))]
use tauri_plugin_deep_link::DeepLinkExt;

const DESKTOP_TRAY_ID: &str = "main-tray";
const APP_CLOSE_REQUESTED_EVENT: &str = "dbx-app-close-requested";
const STARTUP_PROBE_LOG_FILE: &str = "startup.log";
const STARTUP_PROBE_LOG_DIR_ENV: &str = "DBX_STARTUP_LOG_DIR";
const STARTUP_PROBE_KEEP_ENV: &str = "DBX_KEEP_STARTUP_LOG";
const DIAGNOSTIC_MODE_INDEX_ENV: &str = "DBX_DIAGNOSTIC_MODE_INDEX";
const WINDOWS_APP_DATA_DIR_NAME: &str = "com.dbx.app";
const STARTUP_PROBE_MAX_RUN_EVENTS: usize = 80;
const DIAGNOSTIC_STARTUP_PROBE_ALWAYS_KEEP: bool = true;
static STARTUP_PROBE_STATE: Mutex<StartupProbeState> = Mutex::new(StartupProbeState::new());
#[cfg(target_os = "windows")]
static DIAGNOSTIC_SUCCESS_MESSAGE_SHOWN: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
const WEBVIEW2_NO_SANDBOX_ENV: &str = "DBX_WEBVIEW2_NO_SANDBOX";
#[cfg(target_os = "windows")]
const WEBVIEW2_DIAGNOSTIC_USER_DATA_DIR_NAME: &str = "webview2-diagnostic";
#[cfg(target_os = "macos")]
const APP_MENU_QUIT_ID: &str = "app-menu-quit";
#[cfg(target_os = "macos")]
const APP_MENU_COPY_SUPPORT_INFO_ID: &str = "app-menu-copy-support-info";

pub struct CloseBehaviorState {
    confirmed_exit: AtomicBool,
    frontend_ready: AtomicBool,
}

struct StartupProbeState {
    active: bool,
    run_event_count: usize,
}

impl StartupProbeState {
    const fn new() -> Self {
        Self { active: false, run_event_count: 0 }
    }

    fn activate(&mut self) {
        self.active = true;
        self.run_event_count = 0;
    }

    fn deactivate(&mut self) {
        self.active = false;
    }

    fn reserve_run_event(&mut self, max_events: usize) -> Option<(usize, bool)> {
        if !self.active || self.run_event_count >= max_events {
            return None;
        }
        self.run_event_count += 1;
        Some((self.run_event_count, self.run_event_count == max_events))
    }

    fn run_event_count(&self) -> usize {
        self.run_event_count
    }
}

impl CloseBehaviorState {
    fn new() -> Self {
        Self { confirmed_exit: AtomicBool::new(false), frontend_ready: AtomicBool::new(false) }
    }

    pub(crate) fn allow_next_exit(&self) {
        self.confirmed_exit.store(true, Ordering::Relaxed);
    }

    fn take_confirmed_exit(&self) -> bool {
        self.confirmed_exit.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn set_frontend_ready(&self, ready: bool) {
        self.frontend_ready.store(ready, Ordering::Release);
    }

    fn is_frontend_ready(&self) -> bool {
        self.frontend_ready.load(Ordering::Acquire)
    }
}

/// UI language pushed from the frontend i18n layer; native menus follow it and
/// fall back to the OS locale until the first `set_app_locale` call arrives.
pub struct AppLocaleState {
    locale: std::sync::Mutex<Option<String>>,
}

impl AppLocaleState {
    fn new() -> Self {
        Self { locale: std::sync::Mutex::new(None) }
    }

    pub(crate) fn set(&self, locale: String) {
        *self.locale.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(locale);
    }

    fn get(&self) -> String {
        self.locale
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| sys_locale::get_locale().unwrap_or_default())
    }
}
#[cfg(target_os = "macos")]
const MACOS_TRAY_ICON: tauri::image::Image<'_> = tauri::include_image!("icons/tray-macos-template.png");
#[cfg(target_os = "macos")]
const ABOUT_APP_ICON: tauri::image::Image<'_> = tauri::include_image!("icons/icon.png");
#[cfg(not(target_os = "macos"))]
const BLACK_APP_ICON: tauri::image::Image<'_> = tauri::include_image!("icons/icon-black.png");
#[cfg(target_os = "macos")]
const MACOS_DEFAULT_APP_ICON: &[u8] = include_bytes!("../icons/icon.icns");
#[cfg(target_os = "macos")]
const MACOS_DARK_APP_ICON: &[u8] = include_bytes!("../icons/icon-macos-dark.icns");

pub(crate) fn apply_debug_log_level(debug_logging_enabled: bool) {
    log::set_max_level(if debug_logging_enabled { log::LevelFilter::Debug } else { log::LevelFilter::Off });
}

fn should_hide_window_on_close(target_os: &str) -> bool {
    matches!(target_os, "macos" | "windows")
}

fn should_setup_desktop_tray(target_os: &str, show_tray_icon: bool, linux_appindicator_available: bool) -> bool {
    show_tray_icon
        && (matches!(target_os, "macos" | "windows") || (target_os == "linux" && linux_appindicator_available))
}

fn should_enable_single_instance(debug_build: bool) -> bool {
    !debug_build && !diagnostic_startup_mode_enabled()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DiagnosticStartupMode {
    key: &'static str,
    description: &'static str,
    isolated_webview_profile: bool,
    disable_gpu: bool,
    disable_renderer_code_integrity: bool,
    no_sandbox: bool,
    native_decorations: Option<bool>,
}

const DIAGNOSTIC_STARTUP_MODES: &[DiagnosticStartupMode] = &[
    DiagnosticStartupMode {
        key: "default-native",
        description: "default WebView2 environment with native window chrome",
        isolated_webview_profile: false,
        disable_gpu: false,
        disable_renderer_code_integrity: false,
        no_sandbox: false,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-native",
        description: "isolated WebView2 profile with native window chrome",
        isolated_webview_profile: true,
        disable_gpu: false,
        disable_renderer_code_integrity: false,
        no_sandbox: false,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-disable-gpu-native",
        description: "isolated WebView2 profile and disabled GPU rendering",
        isolated_webview_profile: true,
        disable_gpu: true,
        disable_renderer_code_integrity: false,
        no_sandbox: false,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-renderer-code-integrity-native",
        description: "isolated WebView2 profile, disabled GPU rendering, and disabled RendererCodeIntegrity",
        isolated_webview_profile: true,
        disable_gpu: true,
        disable_renderer_code_integrity: true,
        no_sandbox: false,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-no-sandbox-native",
        description: "isolated WebView2 profile and disabled WebView2 sandbox",
        isolated_webview_profile: true,
        disable_gpu: false,
        disable_renderer_code_integrity: false,
        no_sandbox: true,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-all-webview-compat-native",
        description: "isolated WebView2 profile with GPU, RendererCodeIntegrity, and sandbox disabled",
        isolated_webview_profile: true,
        disable_gpu: true,
        disable_renderer_code_integrity: true,
        no_sandbox: true,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "default-profile-disable-gpu-native",
        description: "default WebView2 profile and disabled GPU rendering",
        isolated_webview_profile: false,
        disable_gpu: true,
        disable_renderer_code_integrity: false,
        no_sandbox: false,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "default-profile-all-webview-compat-native",
        description: "default WebView2 profile with GPU, RendererCodeIntegrity, and sandbox disabled",
        isolated_webview_profile: false,
        disable_gpu: true,
        disable_renderer_code_integrity: true,
        no_sandbox: true,
        native_decorations: Some(true),
    },
    DiagnosticStartupMode {
        key: "isolated-profile-all-webview-compat-frameless",
        description: "isolated WebView2 profile with all WebView2 compatibility switches and non-native window chrome",
        isolated_webview_profile: true,
        disable_gpu: true,
        disable_renderer_code_integrity: true,
        no_sandbox: true,
        native_decorations: Some(false),
    },
];

fn diagnostic_startup_mode_index_from_value(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|index| *index < DIAGNOSTIC_STARTUP_MODES.len())
        .unwrap_or(0)
}

fn diagnostic_startup_mode_index() -> usize {
    diagnostic_startup_mode_index_from_value(std::env::var(DIAGNOSTIC_MODE_INDEX_ENV).ok().as_deref())
}

fn diagnostic_startup_mode() -> (usize, &'static DiagnosticStartupMode) {
    let index = diagnostic_startup_mode_index();
    (index, &DIAGNOSTIC_STARTUP_MODES[index])
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn diagnostic_startup_next_mode_index(index: usize) -> Option<usize> {
    (index + 1 < DIAGNOSTIC_STARTUP_MODES.len()).then_some(index + 1)
}

fn diagnostic_startup_mode_enabled() -> bool {
    DIAGNOSTIC_STARTUP_PROBE_ALWAYS_KEEP
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn diagnostic_startup_mode_summary(index: usize, mode: &DiagnosticStartupMode) -> String {
    format!(
        "diagnostic mode {}/{} key={} description=\"{}\" isolated_webview_profile={} disable_gpu={} disable_renderer_code_integrity={} no_sandbox={} native_decorations={:?}",
        index + 1,
        DIAGNOSTIC_STARTUP_MODES.len(),
        mode.key,
        mode.description,
        startup_probe_bool(mode.isolated_webview_profile),
        startup_probe_bool(mode.disable_gpu),
        startup_probe_bool(mode.disable_renderer_code_integrity),
        startup_probe_bool(mode.no_sandbox),
        mode.native_decorations
    )
}

#[cfg(target_os = "macos")]
fn development_dock_badge_label(debug_build: bool) -> Option<&'static str> {
    debug_build.then_some("DEV")
}

#[cfg(target_os = "linux")]
fn linux_appindicator_available() -> bool {
    const APPINDICATOR_LIBRARIES: &[&str] = &["libayatana-appindicator3.so.1", "libappindicator3.so.1"];

    APPINDICATOR_LIBRARIES.iter().any(|library| {
        // tray-icon loads AppIndicator dynamically and panics when neither ABI is
        // installed, so probe the same libraries before entering that code path.
        unsafe { libloading::Library::new(library).is_ok() }
    })
}

#[cfg(not(target_os = "linux"))]
fn linux_appindicator_available() -> bool {
    false
}

#[cfg(test)]
fn uses_application_level_icon(target_os: &str) -> bool {
    target_os == "macos"
}

fn should_show_main_window_after_setup() -> bool {
    true
}

fn should_show_main_window_before_setup_tasks() -> bool {
    true
}

fn startup_probe_log_dir_from_inputs(
    target_os: &str,
    explicit_dir: Option<OsString>,
    windows_appdata: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = explicit_dir.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if target_os == "windows" {
        return windows_appdata
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|dir| dir.join(WINDOWS_APP_DATA_DIR_NAME));
    }
    None
}

fn startup_probe_log_dir() -> Option<PathBuf> {
    startup_probe_log_dir_from_inputs(
        std::env::consts::OS,
        std::env::var_os(STARTUP_PROBE_LOG_DIR_ENV),
        std::env::var_os("APPDATA"),
    )
}

fn startup_probe_log_path() -> Option<PathBuf> {
    startup_probe_log_dir().map(|dir| dir.join(STARTUP_PROBE_LOG_FILE))
}

fn startup_probe_should_keep_after_frontend_ready_from_value(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

fn startup_probe_should_keep_after_frontend_ready() -> bool {
    startup_probe_should_keep_after_frontend_ready_from_value(std::env::var(STARTUP_PROBE_KEEP_ENV).ok().as_deref())
}

fn startup_probe_build_error_message(error: &str) -> String {
    format!("tauri application build failed: {error}")
}

fn startup_probe_bool(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

struct StartupProbeWindowsEnvironmentInput<'a> {
    target_os: &'a str,
    userdomain: Option<&'a str>,
    userdnsdomain: Option<&'a str>,
    computername: Option<&'a str>,
    logonserver: Option<&'a str>,
    sessionname: Option<&'a str>,
    appdata: Option<&'a str>,
    localappdata: Option<&'a str>,
    webview2_additional_args: Option<&'a str>,
    webview2_browser_folder: Option<&'a str>,
    webview2_user_data_folder: Option<&'a str>,
    dbx_webview2_no_sandbox: Option<&'a str>,
    exe_path: Option<&'a std::path::Path>,
}

fn startup_probe_windows_environment_summary_from_values(
    input: StartupProbeWindowsEnvironmentInput<'_>,
) -> Option<String> {
    if input.target_os != "windows" {
        return None;
    }
    let userdomain_non_empty = input.userdomain.is_some_and(|value| !value.trim().is_empty());
    let computername_non_empty = input.computername.is_some_and(|value| !value.trim().is_empty());
    let userdomain_matches_computer = match (input.userdomain, input.computername) {
        (Some(domain), Some(computer)) => domain.eq_ignore_ascii_case(computer),
        _ => false,
    };
    let likely_domain_account = userdomain_non_empty
        && computername_non_empty
        && !userdomain_matches_computer
        && input.userdomain != Some("WORKGROUP");
    let exe_in_program_files = input
        .exe_path
        .and_then(|path| path.to_str())
        .is_some_and(|path| path.to_ascii_lowercase().starts_with("c:\\program files\\"));
    Some(format!(
        "windows environment: userdomain_present={} userdnsdomain_present={} logonserver_present={} likely_domain_account={} session_present={} appdata_present={} localappdata_present={} exe_in_program_files={} webview2_additional_args_present={} webview2_browser_folder_present={} webview2_user_data_folder_present={} dbx_webview2_no_sandbox={}",
        startup_probe_bool(userdomain_non_empty),
        startup_probe_bool(input.userdnsdomain.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(input.logonserver.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(likely_domain_account),
        startup_probe_bool(input.sessionname.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(input.appdata.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(input.localappdata.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(exe_in_program_files),
        startup_probe_bool(input.webview2_additional_args.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(input.webview2_browser_folder.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(input.webview2_user_data_folder.is_some_and(|value| !value.trim().is_empty())),
        startup_probe_bool(matches!(input.dbx_webview2_no_sandbox, Some("1"))),
    ))
}

fn startup_probe_windows_environment_summary() -> Option<String> {
    startup_probe_windows_environment_summary_from_values(StartupProbeWindowsEnvironmentInput {
        target_os: std::env::consts::OS,
        userdomain: std::env::var("USERDOMAIN").ok().as_deref(),
        userdnsdomain: std::env::var("USERDNSDOMAIN").ok().as_deref(),
        computername: std::env::var("COMPUTERNAME").ok().as_deref(),
        logonserver: std::env::var("LOGONSERVER").ok().as_deref(),
        sessionname: std::env::var("SESSIONNAME").ok().as_deref(),
        appdata: std::env::var("APPDATA").ok().as_deref(),
        localappdata: std::env::var("LOCALAPPDATA").ok().as_deref(),
        webview2_additional_args: std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").ok().as_deref(),
        webview2_browser_folder: std::env::var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER").ok().as_deref(),
        webview2_user_data_folder: std::env::var("WEBVIEW2_USER_DATA_FOLDER").ok().as_deref(),
        dbx_webview2_no_sandbox: std::env::var("DBX_WEBVIEW2_NO_SANDBOX").ok().as_deref(),
        exe_path: std::env::current_exe().ok().as_deref(),
    })
}

#[cfg(target_os = "windows")]
fn startup_probe_windows_user_object_name(handle: windows_sys::Win32::Foundation::HANDLE) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    if handle.is_null() {
        return "null".to_string();
    }

    let mut needed = 0u32;
    unsafe {
        let _ = GetUserObjectInformationW(handle, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        let error = unsafe { GetLastError() };
        return format!("unavailable error={error}");
    }

    let mut buffer = vec![0u16; (needed as usize / std::mem::size_of::<u16>()) + 1];
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * std::mem::size_of::<u16>()) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        let error = unsafe { GetLastError() };
        return format!("unavailable error={error}");
    }

    let len = buffer.iter().position(|ch| *ch == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(target_os = "windows")]
fn startup_probe_windows_process_elevation_summary() -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return format!("token_elevated=unknown open_error={}", GetLastError());
        }

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let close_result = CloseHandle(token);
        if ok == 0 {
            return format!(
                "token_elevated=unknown query_error={} token_close={}",
                GetLastError(),
                startup_probe_bool(close_result != 0)
            );
        }

        format!(
            "token_elevated={} token_query_bytes={} token_close={}",
            startup_probe_bool(elevation.TokenIsElevated != 0),
            returned,
            startup_probe_bool(close_result != 0)
        )
    }
}

#[cfg(target_os = "windows")]
fn startup_probe_windows_native_summary() -> Option<String> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::StationsAndDesktops::{GetProcessWindowStation, GetThreadDesktop};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS, SM_REMOTESESSION};

    let mut session_id = 0u32;
    let pid = unsafe { GetCurrentProcessId() };
    let session_result = unsafe { ProcessIdToSessionId(pid, &mut session_id) };
    let session_text = if session_result != 0 {
        session_id.to_string()
    } else {
        format!("unknown error={}", unsafe { GetLastError() })
    };

    let mut startup_info = STARTUPINFOW::default();
    unsafe {
        GetStartupInfoW(&mut startup_info);
    }
    let uses_show_window = (startup_info.dwFlags & STARTF_USESHOWWINDOW) != 0;
    let window_station = unsafe { startup_probe_windows_user_object_name(GetProcessWindowStation()) };
    let desktop = unsafe { startup_probe_windows_user_object_name(GetThreadDesktop(GetCurrentThreadId())) };
    let console_present = unsafe { !GetConsoleWindow().is_null() };
    let monitor_count = unsafe { GetSystemMetrics(SM_CMONITORS) };
    let remote_session = unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 };

    Some(format!(
        "windows native: process_session={} window_station={} desktop={} monitor_count={} remote_session={} console_window_present={} startup_uses_show_window={} startup_show_window={} {}",
        session_text,
        window_station,
        desktop,
        monitor_count,
        startup_probe_bool(remote_session),
        startup_probe_bool(console_present),
        startup_probe_bool(uses_show_window),
        startup_info.wShowWindow,
        startup_probe_windows_process_elevation_summary()
    ))
}

#[cfg(not(target_os = "windows"))]
fn startup_probe_windows_native_summary() -> Option<String> {
    None
}

fn startup_probe_webview_runtime_summary() -> String {
    match tauri::webview_version() {
        Ok(version) => format!("webview runtime version: {version}"),
        Err(error) => format!("webview runtime version unavailable: {error}"),
    }
}

fn app_config_window_labels<R: tauri::Runtime>(app: &tauri::App<R>) -> String {
    let labels = app.config().app.windows.iter().map(|window| window.label.as_str()).collect::<Vec<_>>();
    if labels.is_empty() {
        "config_window_labels=[]".to_string()
    } else {
        format!("config_window_labels=[{}]", labels.join(","))
    }
}

fn app_window_label_state<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> String {
    let mut webview_windows = app.webview_windows().keys().cloned().collect::<Vec<_>>();
    webview_windows.sort();
    format!("webview_window_labels=[{}]", webview_windows.join(","))
}

fn startup_probe_page_load_event_label(event: PageLoadEvent) -> &'static str {
    match event {
        PageLoadEvent::Started => "started",
        PageLoadEvent::Finished => "finished",
    }
}

fn startup_probe_window_event_label(_event: &tauri::WindowEvent) -> &'static str {
    "window-event"
}

fn startup_probe_webview_event_label(_event: &tauri::WebviewEvent) -> &'static str {
    "webview-event"
}

fn startup_probe_run_event_label(event: &RunEvent) -> String {
    match event {
        RunEvent::Ready => "ready".to_string(),
        RunEvent::Resumed => "resumed".to_string(),
        RunEvent::MainEventsCleared => "main-events-cleared".to_string(),
        RunEvent::Exit => "exit".to_string(),
        RunEvent::ExitRequested { code, .. } => format!("exit-requested code={code:?}"),
        RunEvent::WindowEvent { event, .. } => startup_probe_window_event_label(event).to_string(),
        RunEvent::WebviewEvent { event, .. } => startup_probe_webview_event_label(event).to_string(),
        #[cfg(target_os = "macos")]
        RunEvent::Opened { urls } => format!("opened urls={}", urls.len()),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { has_visible_windows, .. } => format!("reopen has_visible_windows={has_visible_windows}"),
        #[cfg(desktop)]
        RunEvent::MenuEvent(_) => "menu-event".to_string(),
        _ => "other".to_string(),
    }
}

fn log_startup_probe_run_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, event: &RunEvent) {
    let Some((count, reached_cap)) = STARTUP_PROBE_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .reserve_run_event(STARTUP_PROBE_MAX_RUN_EVENTS)
    else {
        return;
    };
    let cap_message = if reached_cap { "; further run events omitted" } else { "" };
    append_startup_probe(format!(
        "run event #{count}: {}; {}{cap_message}",
        startup_probe_run_event_label(event),
        main_window_probe_state(app)
    ));
}

fn ensure_startup_probe_parent_dir(path: &std::path::Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    true
}

fn reset_startup_probe() {
    let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.activate();
    if diagnostic_startup_mode_index() > 0 {
        return;
    }
    let Some(path) = startup_probe_log_path() else {
        return;
    };
    if !ensure_startup_probe_parent_dir(&path) {
        return;
    }
    let _ = std::fs::remove_file(path);
}

fn append_startup_probe(message: impl AsRef<str>) {
    let state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.active {
        return;
    }
    let Some(path) = startup_probe_log_path() else {
        return;
    };
    if !ensure_startup_probe_parent_dir(&path) {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(
        file,
        "[{}][pid={}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        std::process::id(),
        message.as_ref()
    );
}

fn install_startup_probe_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!(" line={} column={}", location.line(), location.column()))
            .unwrap_or_default();
        append_startup_probe(format!("panic before frontend ready{location}"));
        default_hook(info);
    }));
}

pub(crate) fn clear_startup_probe_after_frontend_ready() {
    if DIAGNOSTIC_STARTUP_PROBE_ALWAYS_KEEP {
        append_startup_probe("diagnostic startup probe retained after frontend ready");
        show_diagnostic_startup_success_message();
        return;
    }
    let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.deactivate();
    if startup_probe_should_keep_after_frontend_ready() {
        return;
    }
    let Some(path) = startup_probe_log_path() else {
        return;
    };
    let _ = std::fs::remove_file(path);
}

#[cfg(target_os = "windows")]
fn append_webview2_browser_argument(argument: &str) -> bool {
    let mut args = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
    if args.split_whitespace().any(|arg| arg == argument) {
        return false;
    }
    if !args.is_empty() {
        args.push(' ');
    }
    args.push_str(argument);
    std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", args);
    true
}

#[cfg(target_os = "windows")]
fn configure_webview2_sandbox_compat() {
    let (index, mode) = diagnostic_startup_mode();
    let original_args_present =
        std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_some_and(|value| !value.is_empty());
    let original_user_data_present =
        std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_some_and(|value| !value.is_empty());
    let original_no_sandbox_present = std::env::var_os(WEBVIEW2_NO_SANDBOX_ENV).is_some_and(|value| !value.is_empty());
    std::env::remove_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS");
    std::env::remove_var("WEBVIEW2_USER_DATA_FOLDER");
    std::env::remove_var(WEBVIEW2_NO_SANDBOX_ENV);

    append_startup_probe(diagnostic_startup_mode_summary(index, mode));
    append_startup_probe(format!(
        "diagnostic original WebView2 env: additional_args_present={} user_data_folder_present={} dbx_webview2_no_sandbox_present={}",
        startup_probe_bool(original_args_present),
        startup_probe_bool(original_user_data_present),
        startup_probe_bool(original_no_sandbox_present)
    ));

    let mut applied = Vec::new();

    if mode.isolated_webview_profile {
        if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()).map(PathBuf::from) {
            let user_data_dir =
                dir.join(WINDOWS_APP_DATA_DIR_NAME).join(WEBVIEW2_DIAGNOSTIC_USER_DATA_DIR_NAME).join(mode.key);
            let _ = std::fs::create_dir_all(&user_data_dir);
            std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &user_data_dir);
            applied.push(format!("user_data_folder={}", user_data_dir.display()));
        }
    }

    if mode.disable_gpu && append_webview2_browser_argument("--disable-gpu") {
        applied.push("--disable-gpu".to_string());
    }
    if mode.disable_renderer_code_integrity
        && append_webview2_browser_argument("--disable-features=RendererCodeIntegrity")
    {
        applied.push("--disable-features=RendererCodeIntegrity".to_string());
    }
    if mode.no_sandbox && append_webview2_browser_argument("--no-sandbox") {
        applied.push("--no-sandbox".to_string());
    }

    if applied.is_empty() {
        append_startup_probe("diagnostic WebView2 compatibility overrides: none");
    } else {
        append_startup_probe(format!("diagnostic WebView2 compatibility overrides applied: {}", applied.join(" ")));
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_webview2_sandbox_compat() {}

fn startup_probe_run_event_count() -> usize {
    STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).run_event_count()
}

#[cfg(target_os = "windows")]
fn windows_null_terminated(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn open_startup_probe_log_dir_in_explorer() {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let Some(dir) = startup_probe_log_dir() else {
        append_startup_probe("diagnostic fallback skipped opening log directory: no log dir");
        return;
    };
    let operation = windows_null_terminated("open");
    let path = windows_null_terminated(&dir.to_string_lossy());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    append_startup_probe(format!("diagnostic fallback open log directory result={result} dir={}", dir.display()));
}

#[cfg(not(target_os = "windows"))]
fn open_startup_probe_log_dir_in_explorer() {}

#[cfg(target_os = "windows")]
fn show_startup_probe_native_message(reason: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_SYSTEMMODAL,
    };

    let (index, mode) = diagnostic_startup_mode();
    let log_path = startup_probe_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(startup log path unavailable)".to_string());
    let title = windows_null_terminated("DBX startup diagnostic");
    let body = windows_null_terminated(&format!(
        "DBX diagnostic package tried every startup mode and the main window still did not become available.\n\nLast mode: {}/{} ({})\nReason: {reason}\n\nPlease send this file back to the DBX maintainer:\n{log_path}",
        index + 1,
        DIAGNOSTIC_STARTUP_MODES.len(),
        mode.key
    ));
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_SYSTEMMODAL,
        )
    };
    append_startup_probe(format!("diagnostic fallback native message result={result} reason={reason}"));
}

#[cfg(not(target_os = "windows"))]
fn show_startup_probe_native_message(_reason: &str) {}

#[cfg(target_os = "windows")]
fn show_diagnostic_startup_success_message() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND};

    if DIAGNOSTIC_SUCCESS_MESSAGE_SHOWN.swap(true, Ordering::AcqRel) {
        return;
    }
    let (index, mode) = diagnostic_startup_mode();
    let log_path = startup_probe_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(startup log path unavailable)".to_string());
    let title = windows_null_terminated("DBX startup diagnostic");
    let body = windows_null_terminated(&format!(
        "DBX diagnostic package opened the main window successfully.\n\nSuccessful mode: {}/{} ({})\n{}\n\nPlease send this startup log back to the DBX maintainer:\n{log_path}",
        index + 1,
        DIAGNOSTIC_STARTUP_MODES.len(),
        mode.key,
        mode.description
    ));
    let result = unsafe {
        MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND)
    };
    append_startup_probe(format!(
        "diagnostic success native message result={result} mode_index={} mode_key={}",
        index, mode.key
    ));
}

#[cfg(not(target_os = "windows"))]
fn show_diagnostic_startup_success_message() {}

#[cfg(target_os = "windows")]
fn restart_with_next_diagnostic_mode(reason: &str) -> bool {
    let current_index = diagnostic_startup_mode_index();
    let Some(next_index) = diagnostic_startup_next_mode_index(current_index) else {
        append_startup_probe(format!(
            "diagnostic no next startup mode after mode_index={current_index} reason={reason}"
        ));
        return false;
    };
    let Some(next_mode) = DIAGNOSTIC_STARTUP_MODES.get(next_index) else {
        append_startup_probe(format!("diagnostic next startup mode missing index={next_index} reason={reason}"));
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        append_startup_probe(format!("diagnostic restart failed: current_exe unavailable reason={reason}"));
        return false;
    };

    append_startup_probe(format!(
        "diagnostic restarting with next startup mode {}/{} key={} after reason={reason}",
        next_index + 1,
        DIAGNOSTIC_STARTUP_MODES.len(),
        next_mode.key
    ));
    let spawn_result = std::process::Command::new(exe)
        .env(DIAGNOSTIC_MODE_INDEX_ENV, next_index.to_string())
        .env_remove("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS")
        .env_remove("WEBVIEW2_USER_DATA_FOLDER")
        .env_remove(WEBVIEW2_NO_SANDBOX_ENV)
        .spawn();
    match spawn_result {
        Ok(child) => {
            append_startup_probe(format!("diagnostic next startup mode spawned pid={}", child.id()));
            true
        }
        Err(error) => {
            append_startup_probe(format!(
                "diagnostic next startup mode spawn failed index={next_index} key={} error={error}",
                next_mode.key
            ));
            false
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn restart_with_next_diagnostic_mode(_reason: &str) -> bool {
    false
}

fn request_main_window_rebuild<R: tauri::Runtime>(app: &tauri::AppHandle<R>, reason: &str) {
    let app_for_schedule = app.clone();
    let app_for_rebuild = app.clone();
    let reason_for_log = reason.to_string();
    let reason_for_rebuild = reason_for_log.clone();
    let schedule_result = app_for_schedule.run_on_main_thread(move || {
        if app_for_rebuild.get_webview_window("main").is_some() {
            append_startup_probe(format!(
                "diagnostic main window rebuild skipped: main exists reason={reason_for_rebuild}"
            ));
            show_main_window(&app_for_rebuild);
            return;
        }
        let Some(config) = app_for_rebuild.config().app.windows.first().cloned() else {
            append_startup_probe(format!(
                "diagnostic main window rebuild skipped: missing config reason={reason_for_rebuild}"
            ));
            return;
        };
        let build_result =
            tauri::WebviewWindowBuilder::from_config(&app_for_rebuild, &config).and_then(|builder| builder.build());
        match build_result {
            Ok(_) => {
                append_startup_probe(format!("diagnostic main window rebuild succeeded reason={reason_for_rebuild}"));
                show_main_window(&app_for_rebuild);
            }
            Err(error) => {
                append_startup_probe(format!(
                    "diagnostic main window rebuild failed reason={reason_for_rebuild} error={error}"
                ));
            }
        }
    });
    match schedule_result {
        Ok(()) => append_startup_probe(format!("diagnostic main window rebuild scheduled reason={reason_for_log}")),
        Err(error) => append_startup_probe(format!(
            "diagnostic main window rebuild schedule failed reason={reason_for_log} error={error}"
        )),
    }
}

fn start_startup_probe_watchdog<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let app = app.clone();
    std::thread::spawn(move || {
        let mut elapsed = 0_u64;
        let mut fallback_shown = false;
        let mut rebuild_requested = false;
        for delay in [3_u64, 10, 30, 60, 120] {
            std::thread::sleep(Duration::from_secs(delay));
            elapsed += delay;
            let count = startup_probe_run_event_count();
            let main_exists = app.get_webview_window("main").is_some();
            append_startup_probe(format!(
                "watchdog after {elapsed}s: run_event_count={count} main_exists={}; {}",
                startup_probe_bool(main_exists),
                app_window_label_state(&app)
            ));
            if main_exists {
                show_main_window(&app);
            } else if count > 0 && !rebuild_requested {
                rebuild_requested = true;
                request_main_window_rebuild(&app, "event-loop-running-main-window-missing");
            }
            if elapsed >= 13 && !fallback_shown && (!main_exists || count == 0) {
                fallback_shown = true;
                let reason = if count == 0 {
                    "event-loop-produced-no-events"
                } else {
                    "main-window-missing-after-event-loop-start"
                };
                append_startup_probe(format!(
                    "diagnostic startup mode failed mode_index={} reason={reason}",
                    diagnostic_startup_mode_index()
                ));
                if restart_with_next_diagnostic_mode(reason) {
                    append_startup_probe("diagnostic current process exiting after spawning next startup mode");
                    std::process::exit(0);
                } else {
                    show_startup_probe_native_message(reason);
                    open_startup_probe_log_dir_in_explorer();
                }
            }
            if count > 0 && main_exists {
                append_startup_probe("watchdog stopping: event loop and main window observed; show requested");
                return;
            }
        }
    });
}

fn should_confirm_app_exit_request(target_os: &str, exit_code: Option<i32>, confirmed_exit: bool) -> bool {
    should_hide_window_on_close(target_os) && exit_code != Some(tauri::RESTART_EXIT_CODE) && !confirmed_exit
}

fn should_fallback_to_native_quit(target: &str, frontend_ready: bool) -> bool {
    target == "quit" && !frontend_ready
}

fn native_window_decorations_override(target_os: &str) -> Option<bool> {
    match target_os {
        "windows" => diagnostic_startup_mode().1.native_decorations,
        "linux" => Some(false),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn build_app_menu<R: tauri::Runtime>(app_handle: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let pkg_info = app_handle.package_info();
    let app_name = pkg_info.name.clone();
    let about_metadata = AboutMetadata {
        name: Some(app_name.clone()),
        version: Some(pkg_info.version.to_string()),
        copyright: Some(commands::support_info::format_support_info_for_native_about()),
        icon: Some(ABOUT_APP_ICON),
        ..Default::default()
    };
    let copy_support_info_item = MenuItem::with_id(
        app_handle,
        APP_MENU_COPY_SUPPORT_INFO_ID,
        app_menu_copy_support_info_label(&current_app_locale(app_handle)),
        true,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(
        app_handle,
        APP_MENU_QUIT_ID,
        app_menu_quit_label(&current_app_locale(app_handle), &app_name),
        true,
        Some("Cmd+Q"),
    )?;

    Menu::with_items(
        app_handle,
        &[
            &Submenu::with_items(
                app_handle,
                app_name,
                true,
                &[
                    &PredefinedMenuItem::about(app_handle, None, Some(about_metadata))?,
                    &copy_support_info_item,
                    &PredefinedMenuItem::separator(app_handle)?,
                    &PredefinedMenuItem::services(app_handle, None)?,
                    &PredefinedMenuItem::separator(app_handle)?,
                    &PredefinedMenuItem::hide(app_handle, None)?,
                    &PredefinedMenuItem::hide_others(app_handle, None)?,
                    &PredefinedMenuItem::separator(app_handle)?,
                    &quit_item,
                ],
            )?,
            &Submenu::with_items(app_handle, "File", true, &[&PredefinedMenuItem::close_window(app_handle, None)?])?,
            &Submenu::with_items(
                app_handle,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app_handle, None)?,
                    &PredefinedMenuItem::redo(app_handle, None)?,
                    &PredefinedMenuItem::separator(app_handle)?,
                    &PredefinedMenuItem::cut(app_handle, None)?,
                    &PredefinedMenuItem::copy(app_handle, None)?,
                    &PredefinedMenuItem::paste(app_handle, None)?,
                    &PredefinedMenuItem::select_all(app_handle, None)?,
                ],
            )?,
            &Submenu::with_items(app_handle, "View", true, &[&PredefinedMenuItem::fullscreen(app_handle, None)?])?,
            &Submenu::with_items(
                app_handle,
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(app_handle, None)?,
                    &PredefinedMenuItem::maximize(app_handle, None)?,
                    &PredefinedMenuItem::separator(app_handle)?,
                    &PredefinedMenuItem::close_window(app_handle, None)?,
                ],
            )?,
            &Submenu::with_items(app_handle, "Help", true, &[])?,
        ],
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxNvidiaDriver {
    None,
    Nouveau,
    Proprietary,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct LinuxDrmRenderDevice {
    device_file: std::path::PathBuf,
    driver: Option<String>,
    boot_vga: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_nvidia_driver_from_state(
    proprietary_control_exists: bool,
    proprietary_proc_exists: bool,
    render_driver: Option<&str>,
) -> LinuxNvidiaDriver {
    if proprietary_control_exists || proprietary_proc_exists {
        LinuxNvidiaDriver::Proprietary
    } else if render_driver.is_some_and(|driver| driver.eq_ignore_ascii_case("nouveau")) {
        LinuxNvidiaDriver::Nouveau
    } else {
        LinuxNvidiaDriver::None
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_selected_drm_render_device<'a>(
    explicit_device_file: Option<&std::path::Path>,
    devices: &'a [LinuxDrmRenderDevice],
) -> Option<&'a LinuxDrmRenderDevice> {
    if let Some(explicit_device_file) = explicit_device_file {
        // WebKit gives this environment override precedence over EGL/DRM discovery.
        return devices.iter().find(|device| device.device_file.as_path() == explicit_device_file);
    }
    // Before WebKit initializes EGL, boot_vga is the best available default-display signal.
    // The sorted first render node mirrors WebKit's final DRM-device fallback.
    devices.iter().find(|device| device.boot_vga).or_else(|| devices.first())
}

#[cfg(target_os = "linux")]
fn linux_drm_render_devices() -> Vec<LinuxDrmRenderDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut devices = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let node_name = entry.file_name();
            let node_name = node_name.to_str()?;
            let render_index = node_name.strip_prefix("renderD")?;
            if render_index.is_empty() || !render_index.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let device_path = entry.path().join("device");
            let driver = std::fs::read_link(device_path.join("driver"))
                .ok()
                .and_then(|path| path.file_name().and_then(std::ffi::OsStr::to_str).map(str::to_ascii_lowercase));
            let boot_vga = std::fs::read_to_string(device_path.join("boot_vga")).is_ok_and(|value| value.trim() == "1");
            Some(LinuxDrmRenderDevice {
                device_file: std::path::Path::new("/dev/dri").join(node_name),
                driver,
                boot_vga,
            })
        })
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| left.device_file.cmp(&right.device_file));
    devices
}

#[cfg(target_os = "linux")]
fn linux_nvidia_driver() -> LinuxNvidiaDriver {
    let devices = linux_drm_render_devices();
    let explicit_device_file = std::env::var_os("WEBKIT_WEB_RENDER_DEVICE_FILE")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        // Resolve stable /dev/dri/by-path links to the renderD* node used by sysfs.
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
    let render_driver = linux_selected_drm_render_device(explicit_device_file.as_deref(), &devices)
        .and_then(|device| device.driver.as_deref());
    linux_nvidia_driver_from_state(
        std::path::Path::new("/dev/nvidiactl").exists(),
        std::path::Path::new("/proc/driver/nvidia/version").exists(),
        render_driver,
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_webkit_rendering_workarounds(driver: LinuxNvidiaDriver) -> &'static [(&'static str, &'static str)] {
    match driver {
        LinuxNvidiaDriver::Proprietary => {
            // NVIDIA's proprietary driver needs both DMABuf and explicit-sync
            // workarounds to avoid blank windows and compositor failures.
            &[("WEBKIT_DISABLE_DMABUF_RENDERER", "1"), ("__NV_DISABLE_EXPLICIT_SYNC", "1")]
        }
        LinuxNvidiaDriver::Nouveau => {
            // WebKitGTK's DMABuf renderer can produce a fully black WebView on
            // Nouveau while the DOM remains interactive.
            &[("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]
        }
        LinuxNvidiaDriver::None => {
            // AMD / Intel and other Mesa drivers keep DMABuf enabled to avoid
            // unnecessary CPU usage and UI lag on Wayland.
            &[]
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_system_gtk3_immodules_cache_path() -> Option<&'static str> {
    [
        "/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache",
        "/usr/lib/aarch64-linux-gnu/gtk-3.0/3.0.0/immodules.cache",
        "/usr/lib64/gtk-3.0/3.0.0/immodules.cache",
        "/usr/lib/gtk-3.0/3.0.0/immodules.cache",
    ]
    .iter()
    .copied()
    .find(|path| std::path::Path::new(path).is_file())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_appimage_wayland_backend_override(
    appimage: Option<&std::ffi::OsStr>,
    wayland_display: Option<&std::ffi::OsStr>,
    gdk_backend: Option<&std::ffi::OsStr>,
) -> Option<&'static str> {
    if appimage.is_some() && wayland_display.is_some() && gdk_backend.is_none() {
        // AppImage uses the host GTK/WebKitGTK stack. Prefer XWayland for the
        // affected Wayland/EGL path, but keep Wayland and other compiled
        // backends as fallbacks for systems without XWayland.
        Some("x11,wayland,*")
    } else {
        None
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_appimage_system_gtk_immodules_cache(
    appimage: Option<&std::ffi::OsStr>,
    appdir: Option<&std::ffi::OsStr>,
    gtk_im_module: Option<&std::ffi::OsStr>,
    gtk_im_module_file: Option<&std::ffi::OsStr>,
    system_cache_path: Option<&'static str>,
) -> Option<&'static str> {
    let system_cache_path = system_cache_path?;
    if appimage.is_none() || gtk_im_module.is_none() {
        return None;
    }

    let Some(gtk_im_module_file) = gtk_im_module_file else {
        return Some(system_cache_path);
    };
    let appdir = appdir?;

    if std::path::Path::new(gtk_im_module_file).starts_with(std::path::Path::new(appdir)) {
        Some(system_cache_path)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_webkit_rendering_workarounds() {
    for (key, value) in linux_webkit_rendering_workarounds(linux_nvidia_driver()) {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
    if let Some(gdk_backend) = linux_appimage_wayland_backend_override(
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("GDK_BACKEND").as_deref(),
    ) {
        std::env::set_var("GDK_BACKEND", gdk_backend);
    }
    if let Some(gtk_im_module_file) = linux_appimage_system_gtk_immodules_cache(
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::var_os("APPDIR").as_deref(),
        std::env::var_os("GTK_IM_MODULE").as_deref(),
        std::env::var_os("GTK_IM_MODULE_FILE").as_deref(),
        linux_system_gtk3_immodules_cache_path(),
    ) {
        // linuxdeploy-plugin-gtk points GTK_IM_MODULE_FILE at the bundled
        // cache. That hides host IM modules such as fcitx5/ibus, so prefer the
        // host GTK cache when the user has configured a GTK input method.
        std::env::set_var("GTK_IM_MODULE_FILE", gtk_im_module_file);
    }
}

fn show_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        append_startup_probe("show main window skipped: main_window=missing");
        return;
    };
    append_startup_probe(format!("show main window requested; before {}", main_window_probe_state(app)));
    let show_result = window.show().map(|_| "ok".to_string()).unwrap_or_else(|error| format!("error={error}"));
    append_startup_probe(format!("main window show result: {show_result}; {}", main_window_probe_state(app)));
    let unminimize_result =
        window.unminimize().map(|_| "ok".to_string()).unwrap_or_else(|error| format!("error={error}"));
    append_startup_probe(format!(
        "main window unminimize result: {unminimize_result}; {}",
        main_window_probe_state(app)
    ));
    let focus_result = window.set_focus().map(|_| "ok".to_string()).unwrap_or_else(|error| format!("error={error}"));
    append_startup_probe(format!("main window focus result: {focus_result}; {}", main_window_probe_state(app)));
}

fn main_window_probe_state<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> String {
    let Some(window) = app.get_webview_window("main") else {
        return format!("main_window=missing; {}", app_window_label_state(app));
    };
    format!(
        "main_window visible={:?} focused={:?} minimized={:?} maximized={:?} fullscreen={:?} position={:?} outer_size={:?} inner_size={:?}; {}",
        window.is_visible(),
        window.is_focused(),
        window.is_minimized(),
        window.is_maximized(),
        window.is_fullscreen(),
        window.outer_position(),
        window.outer_size(),
        window.inner_size(),
        app_window_label_state(app)
    )
}

fn prepare_main_window_for_display<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    append_startup_probe(format!("prepare main window for display; before {}", main_window_probe_state(app)));
    if let Some(decorations) = native_window_decorations_override(std::env::consts::OS) {
        if let Some(window) = app.get_webview_window("main") {
            let result =
                window.set_decorations(decorations).map(|_| "ok".to_string()).unwrap_or_else(|e| format!("error={e}"));
            append_startup_probe(format!("main window set_decorations result: {result}"));
        } else {
            append_startup_probe("main window set_decorations skipped: main_window=missing");
        }
    }
    window_state_guard::enforce_main_window_bounds(app);
    append_startup_probe(format!("prepare main window for display finished; after {}", main_window_probe_state(app)));
}

fn clear_main_webview_focus<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval(
            r#"
            (() => {
              const active = document.activeElement;
              if (active instanceof HTMLElement) active.blur();
              if (document.body) {
                if (!document.body.hasAttribute("tabindex")) {
                  document.body.setAttribute("tabindex", "-1");
                }
                document.body.focus({ preventScroll: true });
              }
            })();
            "#,
        );
    }
}

pub(crate) fn hide_main_window_for_close<R: tauri::Runtime>(app: &tauri::AppHandle<R>, window: &tauri::Window<R>) {
    clear_main_webview_focus(app);

    #[cfg(target_os = "macos")]
    {
        if window.is_fullscreen().unwrap_or(false) {
            let app = app.clone();
            let window = window.clone();
            let _ = window.set_fullscreen(false);
            tauri::async_runtime::spawn(async move {
                for _ in 0..40 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if !window.is_fullscreen().unwrap_or(false) {
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        let app_to_hide = app.clone();
                        let window_to_hide = window.clone();
                        let _ = app.run_on_main_thread(move || {
                            let _ = window_to_hide.hide();
                            let _ = app_to_hide.hide();
                        });
                        return;
                    }
                }
                let app_to_hide = app.clone();
                let window_to_hide = window.clone();
                let _ = app.run_on_main_thread(move || {
                    let _ = window_to_hide.hide();
                    let _ = app_to_hide.hide();
                });
            });
            return;
        }
    }

    let _ = window.hide();
}

pub(crate) fn request_app_close<R: tauri::Runtime>(app: &tauri::AppHandle<R>, target: &str) {
    let frontend_ready = app.try_state::<CloseBehaviorState>().is_some_and(|state| state.is_frontend_ready());
    if should_fallback_to_native_quit(target, frontend_ready) {
        // A missing WebView2 runtime can prevent the frontend listener from ever
        // loading. Only the explicit tray Quit fallback bypasses the prompt.
        if let Some(state) = app.try_state::<CloseBehaviorState>() {
            state.allow_next_exit();
        }
        app.exit(0);
        return;
    }
    show_main_window(app);
    let _ = app.emit(APP_CLOSE_REQUESTED_EVENT, target);
}

fn open_connection_deep_links(app: &tauri::AppHandle, links: Vec<String>) {
    if links.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<commands::deep_link::DeepLinkOpenState>() {
        state.push(links.clone());
    }
    let _ = app.emit("dbx-open-connection-links", links);
    show_main_window(app);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocaleFamily {
    English,
    SimplifiedChinese,
    TraditionalChinese,
    Japanese,
    Korean,
    Spanish,
    Italian,
    Portuguese,
}

// Mirrors the frontend language mapping in apps/desktop/src/i18n/index.ts
// (localeFromLanguageTag) so native menus agree with the UI language.
fn locale_family(locale: &str) -> LocaleFamily {
    let normalized = locale.replace('_', "-").to_ascii_lowercase();
    let is_language = |language: &str| normalized == language || normalized.starts_with(&format!("{language}-"));
    if is_language("zh") {
        if normalized.contains("hant")
            || normalized.starts_with("zh-tw")
            || normalized.starts_with("zh-hk")
            || normalized.starts_with("zh-mo")
        {
            LocaleFamily::TraditionalChinese
        } else {
            LocaleFamily::SimplifiedChinese
        }
    } else if is_language("ja") {
        LocaleFamily::Japanese
    } else if is_language("ko") {
        LocaleFamily::Korean
    } else if is_language("es") {
        LocaleFamily::Spanish
    } else if is_language("it") {
        LocaleFamily::Italian
    } else if is_language("pt") {
        LocaleFamily::Portuguese
    } else {
        LocaleFamily::English
    }
}

fn tray_menu_labels_for_locale(locale: &str) -> (&'static str, &'static str) {
    match locale_family(locale) {
        LocaleFamily::SimplifiedChinese => ("显示 DBX", "退出 DBX"),
        LocaleFamily::TraditionalChinese => ("顯示 DBX", "退出 DBX"),
        LocaleFamily::Japanese => ("DBXを表示", "DBXを終了"),
        LocaleFamily::Korean => ("DBX 표시", "DBX 종료"),
        LocaleFamily::Spanish => ("Mostrar DBX", "Salir de DBX"),
        LocaleFamily::Italian => ("Mostra DBX", "Esci da DBX"),
        LocaleFamily::Portuguese => ("Mostrar DBX", "Sair do DBX"),
        LocaleFamily::English => ("Show DBX", "Quit DBX"),
    }
}

// Matches the frontend supportInfoCopy translations in apps/desktop/src/i18n/locales/*.ts.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn app_menu_copy_support_info_label(locale: &str) -> &'static str {
    match locale_family(locale) {
        LocaleFamily::SimplifiedChinese => "复制支持信息",
        LocaleFamily::TraditionalChinese => "複製支援資訊",
        LocaleFamily::Japanese => "サポート情報をコピー",
        LocaleFamily::Korean => "지원 정보 복사",
        LocaleFamily::Spanish => "Copiar información",
        LocaleFamily::Italian => "Copia informazioni",
        LocaleFamily::Portuguese => "Copiar informações",
        LocaleFamily::English => "Copy Support Info",
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn app_menu_quit_label(locale: &str, app_name: &str) -> String {
    match locale_family(locale) {
        LocaleFamily::SimplifiedChinese | LocaleFamily::TraditionalChinese => format!("退出 {app_name}"),
        LocaleFamily::Japanese => format!("{app_name}を終了"),
        LocaleFamily::Korean => format!("{app_name} 종료"),
        LocaleFamily::Spanish => format!("Salir de {app_name}"),
        LocaleFamily::Italian => format!("Esci da {app_name}"),
        LocaleFamily::Portuguese => format!("Sair do {app_name}"),
        LocaleFamily::English => format!("Quit {app_name}"),
    }
}

fn current_app_locale<R: tauri::Runtime, M: Manager<R>>(manager: &M) -> String {
    match manager.try_state::<AppLocaleState>() {
        Some(state) => state.get(),
        None => sys_locale::get_locale().unwrap_or_default(),
    }
}

fn build_tray_menu<R: tauri::Runtime, M: Manager<R>>(manager: &M) -> tauri::Result<tauri::menu::Menu<R>> {
    let (show_label, quit_label) = tray_menu_labels_for_locale(&current_app_locale(manager));
    MenuBuilder::new(manager).text("show", show_label).separator().text("quit", quit_label).build()
}

/// Rebuilds the tray menu (and the macOS app menu) so native labels follow the
/// UI language after the frontend reports a locale change.
pub(crate) fn refresh_native_menus(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id(DESKTOP_TRAY_ID) {
        tray.set_menu(Some(build_tray_menu(app)?))?;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app.set_menu(build_app_menu(app)?)?;
    }
    Ok(())
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn setup_desktop_tray<R: tauri::Runtime, M: Manager<R>>(
    manager: &M,
    _icon_theme: DesktopIconTheme,
) -> tauri::Result<()> {
    let menu = build_tray_menu(manager)?;
    let mut tray =
        TrayIconBuilder::<R>::with_id(DESKTOP_TRAY_ID).tooltip("DBX").menu(&menu).show_menu_on_left_click(false);
    #[cfg(target_os = "macos")]
    {
        tray = tray.icon(MACOS_TRAY_ICON).icon_as_template(true);
    }
    #[cfg(target_os = "windows")]
    {
        let icon = match _icon_theme {
            DesktopIconTheme::Default => manager.app_handle().default_window_icon().cloned(),
            DesktopIconTheme::Black => Some(BLACK_APP_ICON),
        };
        if let Some(icon) = icon {
            tray = tray.icon(icon);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(icon) = manager.app_handle().default_window_icon().cloned() {
            tray = tray.icon(icon);
        }
    }

    tray.on_menu_event(|app, event| {
        if event.id() == "show" {
            show_main_window(app);
        } else if event.id() == "quit" {
            request_app_close(app, "quit");
        }
    })
    .on_tray_icon_event(|tray, event| match event {
        TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => show_main_window(tray.app_handle()),
        _ => {}
    })
    .build(manager)?;

    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_macos_app_icon_theme(app: &tauri::AppHandle, icon_theme: DesktopIconTheme) -> tauri::Result<()> {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let icon_bytes = match icon_theme {
        DesktopIconTheme::Default => MACOS_DEFAULT_APP_ICON,
        DesktopIconTheme::Black => MACOS_DARK_APP_ICON,
    };
    app.run_on_main_thread(move || {
        // macOS has no per-window icon. Update NSApplication so the Dock and
        // app switcher reflect the selected theme immediately.
        let marker = unsafe { MainThreadMarker::new_unchecked() };
        let application = NSApplication::sharedApplication(marker);
        let data = NSData::with_bytes(icon_bytes);
        if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) {
            unsafe { application.setApplicationIconImage(Some(&icon)) };
        } else {
            log::warn!("Failed to decode the selected macOS application icon");
        }
    })
}

#[cfg(target_os = "macos")]
fn apply_macos_development_dock_badge(app: &tauri::AppHandle) -> tauri::Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let badge_label = development_dock_badge_label(cfg!(debug_assertions));
    app.run_on_main_thread(move || {
        let marker = unsafe { MainThreadMarker::new_unchecked() };
        let application = NSApplication::sharedApplication(marker);
        let badge_label = badge_label.map(NSString::from_str);
        application.dockTile().setBadgeLabel(badge_label.as_deref());
    })
}

fn apply_desktop_icon_theme(app: &tauri::AppHandle, icon_theme: DesktopIconTheme) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        apply_macos_app_icon_theme(app, icon_theme)
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(window) = app.get_webview_window("main") {
        match icon_theme {
            DesktopIconTheme::Default => {
                if let Some(icon) = app.default_window_icon().cloned() {
                    window.set_icon(icon)?;
                }
            }
            DesktopIconTheme::Black => window.set_icon(BLACK_APP_ICON)?,
        }
    }
    #[cfg(not(target_os = "macos"))]
    Ok(())
}

fn apply_desktop_tray_icon_theme(app: &tauri::AppHandle, _icon_theme: DesktopIconTheme) -> tauri::Result<()> {
    if let Some(_tray) = app.tray_by_id(DESKTOP_TRAY_ID) {
        #[cfg(target_os = "windows")]
        {
            let icon = match _icon_theme {
                DesktopIconTheme::Default => app.default_window_icon().cloned(),
                DesktopIconTheme::Black => Some(BLACK_APP_ICON),
            };
            _tray.set_icon(icon)?;
        }
        #[cfg(target_os = "linux")]
        {
            let icon = match _icon_theme {
                DesktopIconTheme::Default => app.default_window_icon().cloned(),
                DesktopIconTheme::Black => Some(BLACK_APP_ICON),
            };
            _tray.set_icon(icon)?;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = (_tray, _icon_theme);
        }
    }
    Ok(())
}

pub(crate) fn apply_desktop_settings(app: &tauri::AppHandle, desktop_settings: &DesktopSettings) -> tauri::Result<()> {
    apply_debug_log_level(desktop_settings.debug_logging_enabled);
    apply_desktop_icon_theme(app, desktop_settings.icon_theme)?;
    if should_setup_desktop_tray(std::env::consts::OS, desktop_settings.show_tray_icon, linux_appindicator_available())
    {
        if let Some(tray) = app.tray_by_id(DESKTOP_TRAY_ID) {
            tray.set_visible(desktop_settings.show_tray_icon)?;
            apply_desktop_tray_icon_theme(app, desktop_settings.icon_theme)?;
        } else if desktop_settings.show_tray_icon {
            setup_desktop_tray(app, desktop_settings.icon_theme)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{
        app_menu_copy_support_info_label, app_menu_quit_label, diagnostic_startup_mode_index_from_value,
        diagnostic_startup_next_mode_index, linux_appimage_system_gtk_immodules_cache,
        linux_appimage_wayland_backend_override, linux_nvidia_driver_from_state, linux_selected_drm_render_device,
        linux_webkit_rendering_workarounds, native_window_decorations_override, should_confirm_app_exit_request,
        should_enable_single_instance, should_fallback_to_native_quit, should_hide_window_on_close,
        should_setup_desktop_tray, should_show_main_window_after_setup, should_show_main_window_before_setup_tasks,
        startup_probe_build_error_message, startup_probe_log_dir_from_inputs,
        startup_probe_should_keep_after_frontend_ready_from_value, startup_probe_webview_event_label,
        startup_probe_window_event_label, startup_probe_windows_environment_summary_from_values,
        tray_menu_labels_for_locale, uses_application_level_icon, LinuxDrmRenderDevice, LinuxNvidiaDriver,
        StartupProbeState, StartupProbeWindowsEnvironmentInput, DIAGNOSTIC_STARTUP_MODES, WINDOWS_APP_DATA_DIR_NAME,
    };
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    const TEST_GTK3_IMMODULES_CACHE: &str = "/usr/lib/test/gtk-3.0/3.0.0/immodules.cache";

    #[test]
    fn tray_menu_labels_follow_locale() {
        assert_eq!(tray_menu_labels_for_locale("zh-CN"), ("显示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh_CN"), ("显示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh-Hans-CN"), ("显示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh"), ("显示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh-TW"), ("顯示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh-Hant-HK"), ("顯示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("zh-MO"), ("顯示 DBX", "退出 DBX"));
        assert_eq!(tray_menu_labels_for_locale("ja-JP"), ("DBXを表示", "DBXを終了"));
        assert_eq!(tray_menu_labels_for_locale("ko-KR"), ("DBX 표시", "DBX 종료"));
        assert_eq!(tray_menu_labels_for_locale("es-ES"), ("Mostrar DBX", "Salir de DBX"));
        assert_eq!(tray_menu_labels_for_locale("it-IT"), ("Mostra DBX", "Esci da DBX"));
        assert_eq!(tray_menu_labels_for_locale("pt-BR"), ("Mostrar DBX", "Sair do DBX"));
        assert_eq!(tray_menu_labels_for_locale("en-US"), ("Show DBX", "Quit DBX"));
        // Unknown and empty locales fall back to English; "ita" must not match "it".
        assert_eq!(tray_menu_labels_for_locale("ita"), ("Show DBX", "Quit DBX"));
        assert_eq!(tray_menu_labels_for_locale(""), ("Show DBX", "Quit DBX"));
    }

    #[test]
    fn app_menu_labels_follow_locale() {
        assert_eq!(app_menu_quit_label("zh-CN", "DBX"), "退出 DBX");
        assert_eq!(app_menu_quit_label("zh-TW", "DBX"), "退出 DBX");
        assert_eq!(app_menu_quit_label("ja-JP", "DBX"), "DBXを終了");
        assert_eq!(app_menu_quit_label("ko-KR", "DBX"), "DBX 종료");
        assert_eq!(app_menu_quit_label("en-US", "DBX"), "Quit DBX");
        assert_eq!(app_menu_quit_label("", "DBX"), "Quit DBX");
        assert_eq!(app_menu_copy_support_info_label("zh-CN"), "复制支持信息");
        assert_eq!(app_menu_copy_support_info_label("zh-TW"), "複製支援資訊");
        assert_eq!(app_menu_copy_support_info_label("ko-KR"), "지원 정보 복사");
        assert_eq!(app_menu_copy_support_info_label("en-US"), "Copy Support Info");
    }

    #[test]
    fn hides_window_on_close_for_windows_and_macos() {
        assert!(should_hide_window_on_close("windows"));
        assert!(should_hide_window_on_close("macos"));
    }

    #[test]
    fn does_not_hide_window_on_close_for_other_platforms() {
        assert!(!should_hide_window_on_close("linux"));
    }

    #[test]
    fn sets_up_desktop_tray_for_windows_macos_and_linux() {
        assert!(should_setup_desktop_tray("windows", true, false));
        assert!(should_setup_desktop_tray("macos", true, false));
        assert!(should_setup_desktop_tray("linux", true, true));
        assert!(!should_setup_desktop_tray("linux", true, false));
        assert!(!should_setup_desktop_tray("windows", false, true));
        assert!(!should_setup_desktop_tray("macos", false, true));
        assert!(!should_setup_desktop_tray("linux", false, true));
    }

    #[test]
    fn diagnostic_build_skips_single_instance_for_auto_restart() {
        assert!(!should_enable_single_instance(true));
        assert!(!should_enable_single_instance(false));
    }

    #[test]
    fn parses_diagnostic_startup_mode_index_safely() {
        assert_eq!(diagnostic_startup_mode_index_from_value(None), 0);
        assert_eq!(diagnostic_startup_mode_index_from_value(Some("0")), 0);
        assert_eq!(diagnostic_startup_mode_index_from_value(Some("2")), 2);
        assert_eq!(diagnostic_startup_mode_index_from_value(Some("999")), 0);
        assert_eq!(diagnostic_startup_mode_index_from_value(Some("bad")), 0);
    }

    #[test]
    fn diagnostic_startup_modes_cover_expected_webview_variants() {
        assert!(DIAGNOSTIC_STARTUP_MODES.len() >= 8);
        assert!(DIAGNOSTIC_STARTUP_MODES.iter().any(|mode| !mode.isolated_webview_profile
            && !mode.disable_gpu
            && !mode.disable_renderer_code_integrity
            && !mode.no_sandbox));
        assert!(DIAGNOSTIC_STARTUP_MODES.iter().any(|mode| mode.isolated_webview_profile
            && mode.disable_gpu
            && mode.disable_renderer_code_integrity
            && mode.no_sandbox
            && mode.native_decorations == Some(true)));
        assert!(DIAGNOSTIC_STARTUP_MODES.iter().any(|mode| mode.native_decorations == Some(false)));
        assert_eq!(diagnostic_startup_next_mode_index(0), Some(1));
        assert_eq!(diagnostic_startup_next_mode_index(DIAGNOSTIC_STARTUP_MODES.len() - 1), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn labels_debug_builds_in_the_macos_dock() {
        assert_eq!(super::development_dock_badge_label(true), Some("DEV"));
        assert_eq!(super::development_dock_badge_label(false), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_tray_icon_remains_a_system_template() {
        // Menu bar template images are intentionally independent from the app
        // icon theme so macOS can recolor them for light and dark menu bars.
        assert_eq!(super::MACOS_TRAY_ICON.width(), 36);
        assert_eq!(super::MACOS_TRAY_ICON.height(), 36);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_icon_themes_use_packaged_dock_assets() {
        use objc2::AllocAnyThread;
        use objc2_app_kit::NSImage;
        use objc2_foundation::NSData;

        assert!(super::MACOS_DEFAULT_APP_ICON.starts_with(b"icns"));
        assert!(super::MACOS_DARK_APP_ICON.starts_with(b"icns"));
        for bytes in [super::MACOS_DEFAULT_APP_ICON, super::MACOS_DARK_APP_ICON] {
            let data = NSData::with_bytes(bytes);
            assert!(NSImage::initWithData(NSImage::alloc(), &data).is_some());
        }
    }

    #[test]
    fn macos_icon_theme_targets_the_application_instead_of_a_window() {
        assert!(uses_application_level_icon("macos"));
        assert!(!uses_application_level_icon("windows"));
        assert!(!uses_application_level_icon("linux"));
    }

    #[test]
    fn shows_main_window_after_regular_startup_setup() {
        assert!(should_show_main_window_after_setup());
    }

    #[test]
    fn shows_main_window_while_startup_setup_continues() {
        assert!(should_show_main_window_before_setup_tasks());
    }

    #[test]
    fn startup_probe_log_dir_prefers_explicit_override() {
        assert_eq!(
            startup_probe_log_dir_from_inputs(
                "windows",
                Some(OsString::from(r"D:\DBXDiagnostics")),
                Some(OsString::from(r"C:\Users\test\AppData\Roaming")),
            ),
            Some(PathBuf::from(r"D:\DBXDiagnostics"))
        );
    }

    #[test]
    fn startup_probe_log_dir_uses_windows_appdata() {
        assert_eq!(
            startup_probe_log_dir_from_inputs("windows", None, Some(OsString::from(r"C:\Users\test\AppData\Roaming")),),
            Some(PathBuf::from(r"C:\Users\test\AppData\Roaming").join(WINDOWS_APP_DATA_DIR_NAME))
        );
    }

    #[test]
    fn startup_probe_log_dir_is_disabled_without_windows_appdata() {
        assert_eq!(startup_probe_log_dir_from_inputs("windows", None, None), None);
        assert_eq!(startup_probe_log_dir_from_inputs("macos", None, Some(OsString::from("/Users/test/Library"))), None);
    }

    #[test]
    fn startup_probe_log_is_kept_only_when_requested() {
        assert!(startup_probe_should_keep_after_frontend_ready_from_value(Some("1")));
        assert!(!startup_probe_should_keep_after_frontend_ready_from_value(None));
        assert!(!startup_probe_should_keep_after_frontend_ready_from_value(Some("true")));
        assert!(!startup_probe_should_keep_after_frontend_ready_from_value(Some("0")));
    }

    #[test]
    fn startup_probe_state_stops_reserving_events_after_frontend_ready() {
        let mut state = StartupProbeState::new();
        assert_eq!(state.reserve_run_event(3), None);
        state.activate();
        assert_eq!(state.reserve_run_event(3), Some((1, false)));
        state.deactivate();
        assert_eq!(state.reserve_run_event(3), None);
    }

    #[test]
    fn startup_probe_run_event_cap_is_total() {
        let mut state = StartupProbeState::new();
        state.activate();
        assert_eq!(state.reserve_run_event(3), Some((1, false)));
        assert_eq!(state.reserve_run_event(3), Some((2, false)));
        assert_eq!(state.reserve_run_event(3), Some((3, true)));
        assert_eq!(state.reserve_run_event(3), None);
    }

    #[test]
    fn startup_probe_drag_drop_events_do_not_log_paths() {
        let window_event = tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop {
            paths: vec![PathBuf::from(r"C:\Users\alice\secret.sql")],
            position: tauri::PhysicalPosition::new(10.0, 20.0),
        });
        let webview_event = tauri::WebviewEvent::DragDrop(tauri::DragDropEvent::Enter {
            paths: vec![PathBuf::from(r"C:\Users\alice\private.db")],
            position: tauri::PhysicalPosition::new(30.0, 40.0),
        });

        assert_eq!(startup_probe_window_event_label(&window_event), "window-event");
        assert_eq!(startup_probe_webview_event_label(&webview_event), "webview-event");
    }

    #[test]
    fn startup_probe_build_error_message_includes_error() {
        assert_eq!(
            startup_probe_build_error_message("webview initialization failed"),
            "tauri application build failed: webview initialization failed"
        );
    }

    #[test]
    fn startup_probe_windows_environment_summary_is_sanitized() {
        let message = startup_probe_windows_environment_summary_from_values(StartupProbeWindowsEnvironmentInput {
            target_os: "windows",
            userdomain: Some("CORP"),
            userdnsdomain: Some("corp.example.test"),
            computername: Some("LAPTOP-123"),
            logonserver: Some("\\\\DC01"),
            sessionname: Some("Console"),
            appdata: Some(r"C:\Users\alice\AppData\Roaming"),
            localappdata: Some(r"C:\Users\alice\AppData\Local"),
            webview2_additional_args: Some("--disable-features=RendererCodeIntegrity"),
            webview2_browser_folder: None,
            webview2_user_data_folder: Some(r"C:\Users\alice\AppData\Local\DBXWebView"),
            dbx_webview2_no_sandbox: Some("1"),
            exe_path: Some(Path::new(r"C:\Program Files\DBX\dbx.exe")),
        })
        .unwrap();

        assert!(message.contains("likely_domain_account=yes"));
        assert!(message.contains("userdnsdomain_present=yes"));
        assert!(message.contains("webview2_additional_args_present=yes"));
        assert!(message.contains("dbx_webview2_no_sandbox=yes"));
        assert!(!message.contains("CORP"));
        assert!(!message.contains("alice"));
        assert!(!message.contains("DC01"));
    }

    #[test]
    fn only_user_requested_app_exit_needs_frontend_confirmation() {
        assert!(should_confirm_app_exit_request("windows", None, false));
        assert!(should_confirm_app_exit_request("macos", Some(0), false));
        assert!(!should_confirm_app_exit_request("windows", Some(0), true));
        assert!(!should_confirm_app_exit_request("windows", Some(tauri::RESTART_EXIT_CODE), false));
        assert!(!should_confirm_app_exit_request("linux", Some(0), false));
    }

    #[test]
    fn only_quit_uses_native_fallback_before_frontend_ready() {
        assert!(should_fallback_to_native_quit("quit", false));
        assert!(!should_fallback_to_native_quit("quit", true));
        assert!(!should_fallback_to_native_quit("settings", false));
    }

    #[test]
    fn overrides_native_window_decorations_for_desktop_platforms() {
        assert_eq!(native_window_decorations_override("windows"), Some(true));
        assert_eq!(native_window_decorations_override("linux"), Some(false));
        assert_eq!(native_window_decorations_override("macos"), None);
    }

    #[test]
    fn classifies_linux_nvidia_driver_from_selected_renderer() {
        assert_eq!(linux_nvidia_driver_from_state(true, false, None), LinuxNvidiaDriver::Proprietary);
        assert_eq!(linux_nvidia_driver_from_state(false, true, None), LinuxNvidiaDriver::Proprietary);
        assert_eq!(linux_nvidia_driver_from_state(true, false, Some("nouveau")), LinuxNvidiaDriver::Proprietary);
        assert_eq!(linux_nvidia_driver_from_state(false, false, Some("nouveau")), LinuxNvidiaDriver::Nouveau);
        assert_eq!(linux_nvidia_driver_from_state(false, false, Some("i915")), LinuxNvidiaDriver::None);
        assert_eq!(linux_nvidia_driver_from_state(false, false, Some("amdgpu")), LinuxNvidiaDriver::None);
        assert_eq!(linux_nvidia_driver_from_state(false, false, None), LinuxNvidiaDriver::None);
    }

    fn drm_render_device(path: &str, driver: &str, boot_vga: bool) -> LinuxDrmRenderDevice {
        LinuxDrmRenderDevice { device_file: PathBuf::from(path), driver: Some(driver.to_string()), boot_vga }
    }

    #[test]
    fn keeps_linux_dmabuf_when_nouveau_is_loaded_but_not_the_default_renderer() {
        let devices = [
            drm_render_device("/dev/dri/renderD128", "i915", true),
            drm_render_device("/dev/dri/renderD129", "nouveau", false),
        ];

        let selected = linux_selected_drm_render_device(None, &devices).unwrap();
        assert_eq!(selected.driver.as_deref(), Some("i915"));
        assert_eq!(linux_nvidia_driver_from_state(false, false, selected.driver.as_deref()), LinuxNvidiaDriver::None);
    }

    #[test]
    fn honors_explicit_webkit_linux_render_device_on_hybrid_gpus() {
        let devices = [
            drm_render_device("/dev/dri/renderD128", "i915", true),
            drm_render_device("/dev/dri/renderD129", "nouveau", false),
        ];

        let selected = linux_selected_drm_render_device(Some(Path::new("/dev/dri/renderD129")), &devices).unwrap();
        assert_eq!(selected.driver.as_deref(), Some("nouveau"));
        assert_eq!(
            linux_nvidia_driver_from_state(false, false, selected.driver.as_deref()),
            LinuxNvidiaDriver::Nouveau
        );

        let devices = [
            drm_render_device("/dev/dri/renderD128", "i915", false),
            drm_render_device("/dev/dri/renderD129", "nouveau", true),
        ];
        let selected = linux_selected_drm_render_device(Some(Path::new("/dev/dri/renderD128")), &devices).unwrap();
        assert_eq!(selected.driver.as_deref(), Some("i915"));
        assert_eq!(linux_nvidia_driver_from_state(false, false, selected.driver.as_deref()), LinuxNvidiaDriver::None);
    }

    #[test]
    fn uses_nouveau_workaround_for_the_default_linux_renderer() {
        let devices = [
            drm_render_device("/dev/dri/renderD128", "amdgpu", false),
            drm_render_device("/dev/dri/renderD129", "nouveau", true),
        ];

        let selected = linux_selected_drm_render_device(None, &devices).unwrap();
        assert_eq!(selected.driver.as_deref(), Some("nouveau"));
        assert_eq!(
            linux_nvidia_driver_from_state(false, false, selected.driver.as_deref()),
            LinuxNvidiaDriver::Nouveau
        );
    }

    #[test]
    fn applies_driver_specific_linux_webkit_rendering_workarounds() {
        assert_eq!(
            linux_webkit_rendering_workarounds(LinuxNvidiaDriver::Proprietary),
            &[("WEBKIT_DISABLE_DMABUF_RENDERER", "1"), ("__NV_DISABLE_EXPLICIT_SYNC", "1")]
        );
        assert_eq!(
            linux_webkit_rendering_workarounds(LinuxNvidiaDriver::Nouveau),
            &[("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]
        );
        assert_eq!(linux_webkit_rendering_workarounds(LinuxNvidiaDriver::None), &[]);
    }

    #[test]
    fn prefers_x11_for_appimage_wayland_when_backend_is_not_user_configured() {
        assert_eq!(
            linux_appimage_wayland_backend_override(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("wayland-0")),
                None
            ),
            Some("x11,wayland,*")
        );
        assert_eq!(
            linux_appimage_wayland_backend_override(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("wayland-0")),
                Some(OsStr::new("wayland"))
            ),
            None
        );
        assert_eq!(linux_appimage_wayland_backend_override(Some(OsStr::new("/tmp/DBX.AppImage")), None, None), None);
        assert_eq!(linux_appimage_wayland_backend_override(None, Some(OsStr::new("wayland-0")), None), None);
    }

    #[test]
    fn prefers_system_gtk_immodules_cache_for_appimage_input_methods() {
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("/tmp/.mount_DBX123")),
                Some(OsStr::new("fcitx5")),
                Some(OsStr::new("/tmp/.mount_DBX123/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache")),
                Some(TEST_GTK3_IMMODULES_CACHE),
            ),
            Some(TEST_GTK3_IMMODULES_CACHE)
        );
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("/tmp/.mount_DBX123")),
                Some(OsStr::new("ibus")),
                None,
                Some(TEST_GTK3_IMMODULES_CACHE),
            ),
            Some(TEST_GTK3_IMMODULES_CACHE)
        );
    }

    #[test]
    fn preserves_external_gtk_immodules_cache_overrides() {
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("/tmp/.mount_DBX123")),
                Some(OsStr::new("fcitx5")),
                Some(OsStr::new("/opt/custom/immodules.cache")),
                Some(TEST_GTK3_IMMODULES_CACHE),
            ),
            None
        );
    }

    #[test]
    fn skips_system_gtk_immodules_cache_without_required_context() {
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                None,
                Some(OsStr::new("/tmp/.mount_DBX123")),
                Some(OsStr::new("fcitx5")),
                Some(OsStr::new("/tmp/.mount_DBX123/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache")),
                Some(TEST_GTK3_IMMODULES_CACHE),
            ),
            None
        );
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("/tmp/.mount_DBX123")),
                None,
                Some(OsStr::new("/tmp/.mount_DBX123/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache")),
                Some(TEST_GTK3_IMMODULES_CACHE),
            ),
            None
        );
        assert_eq!(
            linux_appimage_system_gtk_immodules_cache(
                Some(OsStr::new("/tmp/DBX.AppImage")),
                Some(OsStr::new("/tmp/.mount_DBX123")),
                Some(OsStr::new("fcitx5")),
                Some(OsStr::new("/tmp/.mount_DBX123/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache")),
                None,
            ),
            None
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    reset_startup_probe();
    install_startup_probe_panic_hook();
    append_startup_probe(format!(
        "process start version={} os={} arch={}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    rustls::crypto::aws_lc_rs::default_provider().install_default().expect("Failed to install rustls crypto provider");
    configure_webview2_sandbox_compat();
    append_startup_probe("runtime prerequisites configured");
    if let Some(summary) = startup_probe_windows_environment_summary() {
        append_startup_probe(summary);
    }
    if let Some(summary) = startup_probe_windows_native_summary() {
        append_startup_probe(summary);
    }
    append_startup_probe(startup_probe_webview_runtime_summary());
    #[cfg(target_os = "linux")]
    apply_linux_webkit_rendering_workarounds();

    let startup_begin = Instant::now();

    append_startup_probe("creating tauri builder");
    let builder = tauri::Builder::default();
    append_startup_probe("registering deep-link plugin");
    let builder = builder.plugin(tauri_plugin_deep_link::init());
    append_startup_probe("deep-link plugin registered");
    append_startup_probe("registering clipboard plugin");
    let builder = builder.plugin(tauri_plugin_clipboard_manager::init());
    append_startup_probe("clipboard plugin registered");
    append_startup_probe("registering dialog plugin");
    let builder = builder.plugin(tauri_plugin_dialog::init());
    append_startup_probe("dialog plugin registered");
    append_startup_probe("registering fs plugin");
    let builder = builder.plugin(tauri_plugin_fs::init());
    append_startup_probe("fs plugin registered");

    let builder = if should_enable_single_instance(cfg!(debug_assertions)) {
        append_startup_probe("registering single-instance plugin");
        builder.plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            append_startup_probe("single-instance callback entered");
            let links = commands::deep_link::connection_deep_links_from_args(args.clone());
            open_connection_deep_links(app, links);

            let paths = commands::external_sql::sql_file_paths_from_args(args.clone(), std::path::Path::new(&cwd));
            if !paths.is_empty() {
                if let Some(state) = app.try_state::<commands::external_sql::ExternalSqlOpenState>() {
                    state.push(paths.clone());
                }
                let _ = app.emit("dbx-open-sql-files", paths);
            }

            let db_paths = commands::external_db::db_file_paths_from_args(args, std::path::Path::new(&cwd));
            if !db_paths.is_empty() {
                if let Some(state) = app.try_state::<commands::external_db::ExternalDbOpenState>() {
                    state.push(db_paths.clone());
                }
                let _ = app.emit("dbx-open-db-files", db_paths);
            }
            show_main_window(app);
        }))
    } else {
        append_startup_probe("single-instance plugin skipped for debug build");
        builder
    };
    append_startup_probe("single-instance plugin stage completed");

    append_startup_probe("registering shell plugin");
    let builder = builder.plugin(tauri_plugin_shell::init());
    append_startup_probe("shell plugin registered");
    append_startup_probe("registering updater plugin");
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    append_startup_probe("updater plugin registered");
    append_startup_probe("registering process plugin");
    let builder = builder.plugin(tauri_plugin_process::init());
    append_startup_probe("process plugin registered");
    append_startup_probe("registering window-state plugin");
    let builder = builder.plugin(
        tauri_plugin_window_state::Builder::default()
            .with_state_flags(window_state_guard::persisted_main_window_state_flags())
            .build(),
    );
    append_startup_probe("window-state plugin registered");

    // macOS app menu (Cmd+Q / Dock Quit). Skip on Linux/Windows so an empty menu bar
    // is not installed where there was none before.
    #[cfg(target_os = "macos")]
    let builder = builder.menu(build_app_menu).on_menu_event(|app, event| {
        if event.id() == APP_MENU_QUIT_ID {
            request_app_close(app, "quit");
        } else if event.id() == APP_MENU_COPY_SUPPORT_INFO_ID {
            if let Err(err) = app.clipboard().write_text(commands::support_info::format_support_info_for_clipboard()) {
                log::warn!("Failed to copy support info from app menu: {err}");
            }
        }
    });

    append_startup_probe("configuring tauri application builder");
    let builder = builder
        .manage(CloseBehaviorState::new())
        .manage(AppLocaleState::new())
        .on_page_load(|webview, payload| {
            append_startup_probe(format!(
                "page load {} webview={} url_scheme={} url_path_len={}",
                startup_probe_page_load_event_label(payload.event()),
                webview.label(),
                payload.url().scheme(),
                payload.url().path().len()
            ));
            if payload.event() == PageLoadEvent::Started {
                if let Some(state) = webview.app_handle().try_state::<CloseBehaviorState>() {
                    state.set_frontend_ready(false);
                    append_startup_probe("frontend ready state reset by page load start");
                } else {
                    append_startup_probe("frontend ready state reset skipped: close behavior state missing");
                }
            }
        })
        .setup(move |app| {
            let setup_start = Instant::now();
            eprintln!("[STARTUP] plugins registered in {:?}", startup_begin.elapsed());
            append_startup_probe(format!(
                "setup entered after {:?}; {}",
                startup_begin.elapsed(),
                main_window_probe_state(app.handle())
            ));

            if should_show_main_window_before_setup_tasks() {
                prepare_main_window_for_display(app.handle());
                show_main_window(app.handle());
                append_startup_probe(format!(
                    "early main window show requested; {}",
                    main_window_probe_state(app.handle())
                ));
            }

            append_startup_probe("resolving app data dir");
            let default_data_dir =
                app.path().app_data_dir().map_err(|e| e.to_string()).expect("Failed to resolve app data dir");
            let data_dir_resolution = data_dir::resolve_data_dir_with_mode(default_data_dir);
            let data_dir = data_dir_resolution.data_dir.clone();
            std::fs::create_dir_all(&data_dir).expect("Failed to create data dir");
            append_startup_probe("data dir ready");
            let alternative_data_dir = data_dir::alternative_data_dir(&data_dir_resolution);
            match maybe_import_user_data_db(&data_dir, alternative_data_dir.as_deref()) {
                Ok(result) => eprintln!("[STARTUP] data db fallback import: {result:?}"),
                Err(err) => eprintln!("[STARTUP] data db fallback import failed: {err}"),
            }
            let db_path = data_dir.join("dbx.db");

            let t = Instant::now();
            append_startup_probe("opening storage");
            let storage = tauri::async_runtime::block_on(async {
                let s = Storage::open(&db_path).await.expect("Failed to open storage");
                eprintln!("[STARTUP]   Storage::open in {:?}", t.elapsed());
                append_startup_probe(format!("storage opened in {:?}", t.elapsed()));
                let t2 = Instant::now();
                s.migrate_from_json(&data_dir).await.expect("Failed to migrate JSON data");
                eprintln!("[STARTUP]   migrate_from_json in {:?}", t2.elapsed());
                append_startup_probe(format!("json migration completed in {:?}", t2.elapsed()));
                s
            });
            let desktop_settings = tauri::async_runtime::block_on(storage.load_desktop_settings()).unwrap_or_default();
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                    .format(|out, message, record| {
                        out.finish(format_args!(
                            "[{}][{}][{}] {}",
                            chrono::Local::now().format("%Y-%m-%d][%H:%M:%S%.3f"),
                            record.level(),
                            record.target(),
                            message
                        ));
                    })
                    .level(log::LevelFilter::Debug)
                    .build(),
            )?;
            apply_debug_log_level(desktop_settings.debug_logging_enabled);
            eprintln!("[STARTUP] storage ready in {:?}", t.elapsed());
            append_startup_probe(format!("storage ready in {:?}", t.elapsed()));

            // Initialize core dialect registry and load external plugin dialects
            let dialect_init_start = Instant::now();
            register_core_dialects();
            let registry = DialectRegistry::global();
            let plugin_dirs = vec![data_dir.join("plugins").join("dialects")];
            let load_result = DialectPluginLoader::scan_and_load(registry, &plugin_dirs);
            eprintln!(
                "[STARTUP] dialect plugins loaded: {} success, {} errors, {} skipped in {:?}",
                load_result.loaded.len(),
                load_result.errors.len(),
                load_result.skipped.len(),
                dialect_init_start.elapsed()
            );
            append_startup_probe(format!(
                "dialect plugins loaded: {} success, {} errors, {} skipped in {:?}",
                load_result.loaded.len(),
                load_result.errors.len(),
                load_result.skipped.len(),
                dialect_init_start.elapsed()
            ));

            // Start dialect YAML hot-reload watcher
            let watch_dirs = plugin_dirs.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = DialectHotReload::run_forever(watch_dirs, DialectRegistry::global()).await {
                    log::error!("[STARTUP] dialect hot-reload watcher exited: {e}");
                }
            });
            eprintln!("[STARTUP] dialect hot-reload watcher started");

            let default_agent_dir = data_dir_resolution.uses_custom_data_dir().then(|| data_dir.join("agents"));
            let (plugin_dir, agent_dir) = commands::app_settings::resolve_driver_store_dirs_from_settings(
                &desktop_settings,
                &data_dir,
                default_agent_dir,
            );

            let state = if let Some(agent_dir) = agent_dir {
                AppState::new_with_plugin_and_agent_dir_and_app_version(
                    storage,
                    plugin_dir,
                    agent_dir,
                    env!("CARGO_PKG_VERSION"),
                )
            } else {
                AppState::new_with_plugin_dir_and_app_version(storage, plugin_dir, env!("CARGO_PKG_VERSION"))
            };
            state.set_duckdb_worker_process_isolation_enabled(desktop_settings.duckdb_worker_process_isolation);
            state.set_duckdb_worker_max_processes(desktop_settings.duckdb_worker_max_processes);
            let state = Arc::new(state);
            app.manage(state.clone());
            app.manage(commands::redis_pubsub_server::start_pubsub_server(state.clone()));
            app.manage(commands::saved_sql::SavedSqlStorageState { data_dir: data_dir.clone() });
            app.manage(commands::external_sql::ExternalSqlOpenState::default());
            app.manage(commands::external_db::ExternalDbOpenState::default());
            app.manage(commands::deep_link::DeepLinkOpenState::default());
            app.manage(commands::update::PendingUpdateState::default());
            app.manage(commands::ssh_prompt::SshPromptState::new());
            commands::ssh_prompt::install_ssh_prompt_bridge(app.handle());
            commands::ssh_prompt::install_ssh_notice_bridge(app.handle());
            #[cfg(target_os = "macos")]
            macos_app_delegate::install_dock_quit_handler(app.handle());
            let startup_links = commands::deep_link::connection_deep_links_from_args(std::env::args().skip(1));
            open_connection_deep_links(app.handle(), startup_links);

            let app_handle = app.handle().clone();
            commands::mcp_bridge::start(app_handle, state, data_dir);
            eprintln!("[STARTUP] setup complete in {:?} (total {:?})", setup_start.elapsed(), startup_begin.elapsed());
            append_startup_probe(format!(
                "setup tasks complete in {:?} total {:?}",
                setup_start.elapsed(),
                startup_begin.elapsed()
            ));

            prepare_main_window_for_display(app.handle());
            if should_setup_desktop_tray(
                std::env::consts::OS,
                desktop_settings.show_tray_icon,
                linux_appindicator_available(),
            ) {
                setup_desktop_tray(app, desktop_settings.icon_theme)?;
            }
            apply_desktop_icon_theme(app.handle(), desktop_settings.icon_theme)?;
            #[cfg(target_os = "macos")]
            apply_macos_development_dock_badge(app.handle())?;
            if should_show_main_window_after_setup() {
                show_main_window(app.handle());
                append_startup_probe(format!(
                    "final main window show requested; {}",
                    main_window_probe_state(app.handle())
                ));
            }
            #[cfg(any(windows, target_os = "linux"))]
            let _ = app.deep_link().register_all();

            append_startup_probe(format!("setup finished; {}", main_window_probe_state(app.handle())));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !should_hide_window_on_close(std::env::consts::OS) {
                    return;
                }
                let app = window.app_handle();
                if app.try_state::<CloseBehaviorState>().is_none() {
                    api.prevent_close();
                    hide_main_window_for_close(app, window);
                    return;
                }
                api.prevent_close();
                request_app_close(app, "settings");
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::ai::ai_complete,
            commands::ai::ai_stream,
            commands::ai::ai_agent_stream,
            commands::ai::ai_cancel_stream,
            commands::ai::ai_test_connection,
            commands::ai::ai_list_models,
            commands::ai::ai_resolve_model_effort,
            commands::ai::save_ai_config,
            commands::ai::load_ai_config,
            commands::ai::save_ai_provider_config,
            commands::ai::load_ai_provider_configs,
            commands::ai::save_ai_chat_selection,
            commands::ai::load_ai_chat_selection,
            commands::ai::save_ai_conversation,
            commands::ai::load_ai_conversations,
            commands::ai::delete_ai_conversation,
            commands::ai_multi_config::save_ai_configs,
            commands::ai_multi_config::load_ai_configs,
            commands::ai_multi_config::set_default_ai_config,
            commands::ai_multi_config::save_ai_config_item,
            commands::ai_multi_config::delete_ai_config,
            commands::prompt_template::load_prompt_templates,
            commands::prompt_template::save_prompt_template,
            commands::prompt_template::delete_prompt_template,
            commands::prompt_template::get_ai_global_custom_instructions,
            commands::prompt_template::set_ai_global_custom_instructions,
            commands::app_settings::load_desktop_settings,
            commands::app_settings::save_desktop_settings,
            commands::app_settings::load_max_agent_turns,
            commands::app_settings::save_max_agent_turns,
            commands::app_settings::load_max_retries,
            commands::app_settings::save_max_retries,
            commands::app_settings::set_app_locale,
            commands::app_settings::complete_app_close,
            commands::app_settings::mark_frontend_ready,
            commands::app_settings::request_app_close_from_window_controls,
            commands::window_controls::set_macos_traffic_light_position,
            commands::app_settings::set_driver_store_dir,
            commands::app_settings::set_plugin_store_dir,
            commands::app_settings::set_agent_store_dir,
            commands::app_settings::get_driver_store_path,
            commands::app_settings::load_pinned_tree_node_ids,
            commands::app_settings::save_pinned_tree_node_ids,
            commands::app_settings::load_mcp_global_policy,
            commands::app_settings::save_mcp_global_policy,
            commands::app_settings::load_editor_settings,
            commands::app_settings::save_editor_settings,
            commands::app_settings::load_open_tabs_state,
            commands::app_settings::save_open_tabs_state,
            commands::app_settings::load_saved_sql_editor_positions,
            commands::app_settings::save_saved_sql_editor_positions,
            commands::app_settings::load_native_debug_logs,
            commands::support_info::get_app_support_info,
            commands::cloud_sync::webdav_sync_test,
            commands::cloud_sync::webdav_password_status,
            commands::cloud_sync::save_webdav_saved_password,
            commands::cloud_sync::forget_webdav_saved_password,
            commands::cloud_sync::webdav_sync_secrets_status,
            commands::cloud_sync::save_webdav_sync_secrets_preference,
            commands::cloud_sync::forget_webdav_sync_secrets_passphrase,
            commands::cloud_sync::webdav_sync_upload,
            commands::cloud_sync::webdav_sync_download,
            commands::cloud_sync::snippet_sync_test,
            commands::cloud_sync::snippet_token_status,
            commands::cloud_sync::save_snippet_saved_token,
            commands::cloud_sync::forget_snippet_saved_token,
            commands::cloud_sync::snippet_sync_upload,
            commands::cloud_sync::snippet_sync_download,
            commands::connection::test_connection,
            commands::connection::test_connection_with_info,
            commands::connection::connect_db,
            commands::connection::connection_final_proxy_port,
            commands::connection::disconnect_db,
            commands::connection::close_database_connection,
            commands::connection::refresh_connections,
            commands::connection::check_connection_health,
            commands::connection::connection_identifier_quote,
            commands::connection::connection_database_info,
            commands::connection::save_connection_database_info,
            commands::connection::save_connections,
            commands::connection::load_connections,
            commands::connection::save_sidebar_layout,
            commands::connection::load_sidebar_layout,
            commands::plugins::list_plugins,
            commands::plugins::list_jdbc_drivers,
            commands::plugins::list_jdbc_maven_bundles,
            commands::plugins::list_jdbc_local_bundles,
            commands::plugins::import_jdbc_drivers,
            commands::plugins::install_jdbc_driver_from_maven,
            commands::plugins::install_prestosql_jdbc_driver,
            commands::plugins::delete_jdbc_driver,
            commands::plugins::delete_jdbc_maven_bundle,
            commands::plugins::delete_jdbc_local_bundle,
            commands::plugins::jdbc_plugin_status,
            commands::plugins::install_jdbc_plugin,
            commands::plugins::install_jdbc_plugin_local,
            commands::plugins::uninstall_jdbc_plugin,
            commands::schema::list_databases,
            commands::schema::list_database_storage,
            commands::schema::list_doris_catalogs,
            commands::schema::list_doris_catalog_databases,
            commands::schema::list_sqlserver_linked_servers,
            commands::schema::list_sqlserver_linked_server_catalogs,
            commands::schema::list_sqlserver_linked_server_schemas,
            commands::schema::list_sqlserver_linked_server_tables,
            commands::schema::list_tables,
            commands::schema::get_table_comment,
            commands::schema::list_objects,
            commands::schema::list_object_statistics,
            commands::schema::list_completion_objects,
            commands::schema::completion_assistant_search,
            commands::schema::get_object_source,
            commands::schema::list_schemas,
            commands::schema::list_schema_infos,
            commands::schema::list_data_types,
            commands::schema::get_columns,
            commands::schema::get_sqlserver_column_metadata,
            commands::schema::list_indexes,
            commands::schema::list_foreign_keys,
            commands::schema::list_triggers,
            commands::schema::list_constraints,
            commands::schema::list_partitions,
            commands::schema::list_subpartitions,
            commands::schema::get_table_ddl,
            commands::schema::list_functions,
            commands::schema::list_sequences,
            commands::schema::list_rules,
            commands::schema::list_owners,
            commands::schema::list_extensions,
            commands::schema::list_available_extensions,
            commands::schema_diff::prepare_schema_diff,
            commands::schema_diff::generate_schema_sync_sql,
            commands::dialect_cmd::list_dialect_data_types,
            commands::schema_cache::save_schema_cache,
            commands::schema_cache::load_schema_cache,
            commands::schema_cache::delete_schema_cache_prefix,
            commands::tab_runtime_cache::save_tab_runtime_cache,
            commands::tab_runtime_cache::load_tab_runtime_cache,
            commands::tab_runtime_cache::list_tab_runtime_cache_metadata,
            commands::tab_runtime_cache::prune_tab_runtime_cache,
            commands::tab_runtime_cache::delete_tab_runtime_cache_owner,
            commands::tab_runtime_cache::delete_tab_runtime_cache,
            commands::query::execute_query,
            commands::query::execute_multi,
            commands::query::cancel_query,
            commands::query::close_query_session,
            commands::query::close_client_connection_session,
            commands::query::execute_batch,
            commands::query::execute_script,
            commands::query::execute_in_transaction,
            commands::query::execute_script_with_2pc,
            commands::query::begin_manual_transaction,
            commands::query::execute_in_manual_transaction,
            commands::query::commit_manual_transaction,
            commands::query::rollback_manual_transaction,
            commands::query::analyze_sql_references,
            commands::query::find_statement_at_cursor,
            commands::query::prepare_query_pagination_execution_plan,
            commands::query::build_sorted_query_sql,
            commands::query::build_explain_sql,
            commands::query::get_explain_info,
            commands::query::build_create_user_sql,
            commands::query::build_dropped_file_preview_sql,
            commands::query::build_table_select_sql,
            commands::query::build_database_search_sql,
            commands::query::build_search_result_where,
            commands::query::build_rename_object_sql,
            commands::query::build_create_database_sql,
            #[cfg(feature = "duckdb-sidecar")]
            commands::query::build_duckdb_attach_database_sql,
            commands::query::build_sqlite_attach_database_sql,
            commands::query::build_drop_object_sql,
            commands::query::build_drop_table_sql,
            commands::query::build_drop_table_child_object_sql,
            commands::query::build_empty_table_sql,
            commands::query::build_truncate_table_sql,
            commands::query::build_drop_database_sql,
            commands::query::build_create_schema_sql,
            commands::query::build_update_database_properties_sql,
            commands::query::build_drop_schema_sql,
            commands::query::build_duplicate_table_structure_sql,
            commands::query::build_copy_table_data_sql,
            commands::query::build_executable_object_source_statements,
            commands::query::build_executable_object_source_sql,
            commands::query::build_editable_object_source,
            commands::query::build_routine_rename_object_source_statements,
            commands::query::build_view_ddl_sql,
            commands::query::build_table_structure_change_sql,
            commands::query::preview_sqlite_table_structure_change,
            commands::query::apply_sqlite_table_structure_change,
            commands::query::build_create_table_sql,
            commands::query::build_single_column_alter_sql,
            commands::query::analyze_editable_query_editability,
            commands::query::prepare_data_grid_save,
            commands::query::extract_data_grid_selection,
            commands::query::build_data_grid_copy_update_statements,
            commands::query::build_data_grid_copy_insert_statement,
            commands::query::build_data_grid_context_filter_condition,
            commands::query::build_data_grid_column_value_filter_condition,
            commands::query::build_data_grid_column_values_filter_condition,
            commands::query::build_data_grid_column_distinct_values_sql,
            commands::query::build_data_grid_count_sql,
            commands::query::build_hive_table_properties_sql,
            commands::query::build_export_insert_statements,
            commands::query::build_export_sql_insert,
            commands::query::build_database_sql_export,
            commands::data_compare::prepare_data_compare,
            commands::data_compare::prepare_data_compare_from_tables,
            commands::data_compare::prepare_data_compare_missing_target,
            commands::data_compare::build_data_compare_sync_plan,
            commands::sql_file::preview_sql_file,
            commands::sql_file::execute_sql_file,
            commands::sql_file::execute_sql_files,
            commands::sql_file::cancel_sql_file_execution,
            commands::external_sql::pending_open_sql_files,
            commands::external_sql::read_external_sql_file,
            commands::external_sql::write_external_sql_file,
            commands::external_sql::save_external_sql_file,
            commands::list_sql_files::list_sql_files_in_folder,
            commands::external_db::pending_open_db_files,
            commands::keychain::read_keychain_password,
            commands::keychain::read_keychain_passwords,
            commands::deep_link::pending_open_connection_links,
            commands::table_import::preview_table_import_file,
            commands::table_import::import_table_file,
            commands::table_import::cancel_table_import,
            commands::redis_cmd::redis_list_databases,
            commands::redis_cmd::redis_scan_keys,
            commands::redis_cmd::redis_scan_keys_batch,
            commands::redis_cmd::redis_scan_values,
            commands::redis_cmd::redis_get_value,
            commands::redis_cmd::redis_get_ttl,
            commands::redis_cmd::redis_get_stream_entries,
            commands::redis_cmd::redis_get_stream_groups,
            commands::redis_cmd::redis_get_stream_consumers,
            commands::redis_cmd::redis_get_stream_pending,
            commands::redis_cmd::redis_set_string,
            commands::redis_cmd::redis_delete_key,
            commands::redis_cmd::redis_hash_set,
            commands::redis_cmd::redis_hash_del,
            commands::redis_cmd::redis_list_push,
            commands::redis_cmd::redis_list_set,
            commands::redis_cmd::redis_list_remove,
            commands::redis_cmd::redis_set_add,
            commands::redis_cmd::redis_set_remove,
            commands::redis_cmd::redis_zadd,
            commands::redis_cmd::redis_zrem,
            commands::redis_cmd::redis_stream_add,
            commands::redis_cmd::redis_json_set,
            commands::redis_cmd::redis_check_json_module,
            commands::redis_cmd::redis_set_ttl,
            commands::redis_cmd::redis_set_expire_at,
            commands::redis_cmd::redis_delete_keys,
            commands::redis_cmd::redis_flush_db,
            commands::redis_cmd::redis_execute_command,
            commands::redis_cmd::redis_load_more,
            commands::redis_cmd::redis_pubsub_publish,
            commands::redis_pubsub_server::redis_pubsub_server_port,
            commands::redis_cmd::redis_slowlog_get,
            commands::redis_cmd::redis_cluster_master_nodes,
            commands::etcd_cmd::etcd_supports_ttl,
            commands::etcd_cmd::etcd_list_prefix,
            commands::etcd_cmd::etcd_get,
            commands::etcd_cmd::etcd_put,
            commands::etcd_cmd::etcd_delete,
            commands::etcd_cmd::etcd_rename,
            commands::etcd_cmd::etcd_history,
            commands::etcd_cmd::etcd_status,
            commands::etcd_cmd::etcd_preflight,
            commands::etcd_cmd::etcd_compact,
            commands::etcd_cmd::etcd_defrag,
            commands::etcd_cmd::etcd_watch_start,
            commands::etcd_cmd::etcd_watch_poll,
            commands::etcd_cmd::etcd_watch_stop,
            commands::etcd_cmd::etcd_lease_list,
            commands::etcd_cmd::etcd_lease_call,
            commands::etcd_cmd::etcd_auth_call,
            commands::zookeeper_cmd::zookeeper_list_prefix,
            commands::zookeeper_cmd::zookeeper_get,
            commands::zookeeper_cmd::zookeeper_put,
            commands::zookeeper_cmd::zookeeper_delete,
            commands::nacos_cmd::nacos_test_connection,
            commands::nacos_cmd::nacos_list_namespaces,
            commands::nacos_cmd::nacos_create_namespace,
            commands::nacos_cmd::nacos_update_namespace,
            commands::nacos_cmd::nacos_list_configs,
            commands::nacos_cmd::nacos_get_config,
            commands::nacos_cmd::nacos_publish_config,
            commands::nacos_cmd::nacos_delete_config,
            commands::nacos_cmd::nacos_list_config_history,
            commands::nacos_cmd::nacos_get_config_history,
            commands::nacos_cmd::nacos_rollback_config,
            commands::nacos_cmd::nacos_get_rnacos_console_captcha,
            commands::nacos_cmd::nacos_login_rnacos_console,
            commands::nacos_cmd::nacos_list_services,
            commands::nacos_cmd::nacos_list_instances,
            commands::nacos_cmd::nacos_update_instance,
            commands::nacos_cmd::nacos_get_dashboard,
            commands::nacos_cmd::nacos_raw_request,
            commands::nacos_cmd::nacos_search_config_content,
            commands::nacos_cmd::nacos_cancel_operation,
            commands::nacos_cmd::nacos_export_configs,
            commands::nacos_cmd::nacos_preview_config_import,
            commands::nacos_cmd::nacos_apply_config_import,
            commands::nacos_cmd::nacos_preview_config_transfer,
            commands::nacos_cmd::nacos_apply_config_transfer,
            commands::saved_sql::load_saved_sql_library,
            commands::saved_sql::load_saved_sql_file,
            commands::saved_sql::save_saved_sql_folder,
            commands::saved_sql::delete_saved_sql_folder,
            commands::saved_sql::save_saved_sql_file,
            commands::saved_sql::delete_saved_sql_file,
            commands::saved_sql::saved_sql_storage_dir,
            commands::saved_sql::open_saved_sql_storage_dir,
            commands::saved_sql::sync_saved_sql_directory,
            commands::fs_open::reveal_path_in_file_manager,
            commands::fs_open::is_sqlite_database_file,
            commands::fs_open::delete_database_backup_files,
            commands::sqlite_backup::backup_sqlite_database,
            commands::mongo_cmd::mongo_list_databases,
            commands::mongo_cmd::mongo_list_collections,
            commands::mongo_cmd::vector_collection_detail,
            commands::mongo_cmd::mongo_create_database,
            commands::mongo_cmd::mongo_drop_database,
            commands::mongo_cmd::mongo_drop_collection,
            commands::mongo_cmd::mongo_rename_collection,
            commands::document_cmd::document_list_databases,
            commands::document_cmd::document_list_collections,
            commands::document_cmd::document_find_documents,
            commands::document_cmd::elasticsearch_count_documents,
            commands::document_cmd::document_list_gridfs_buckets,
            commands::document_cmd::document_create_gridfs_bucket,
            commands::document_cmd::document_delete_gridfs_bucket,
            commands::document_cmd::document_list_gridfs_files,
            commands::document_cmd::document_download_gridfs_file,
            commands::document_cmd::document_upload_gridfs_file,
            commands::document_cmd::document_delete_gridfs_file,
            commands::mongo_cmd::mongo_find_documents,
            commands::mongo_cmd::mongo_parse_shell_command,
            commands::mongo_cmd::mongo_find_one,
            commands::mongo_cmd::mongo_count_documents,
            commands::mongo_cmd::mongo_server_version,
            commands::mongo_cmd::mongo_collection_stats,
            commands::mongo_cmd::mongo_aggregate_documents,
            commands::mongo_cmd::mongo_distinct,
            commands::mongo_cmd::mongo_create_index,
            commands::mongo_cmd::mongo_drop_indexes,
            commands::document_cmd::document_insert_document,
            commands::mongo_cmd::mongo_insert_document,
            commands::mongo_cmd::mongo_insert_documents,
            commands::document_cmd::document_update_document,
            commands::mongo_cmd::mongo_update_document,
            commands::mongo_cmd::mongo_update_documents,
            commands::document_cmd::document_delete_document,
            commands::hbase_cmd::hbase_get_table_schema,
            commands::hbase_cmd::hbase_scan_rows,
            commands::hbase_cmd::hbase_get_row,
            commands::hbase_cmd::hbase_put_row,
            commands::hbase_cmd::hbase_delete_row,
            commands::hbase_cmd::hbase_create_table,
            commands::hbase_cmd::hbase_delete_table,
            commands::mongo_cmd::mongo_delete_document,
            commands::mongo_cmd::mongo_delete_documents,
            commands::mongo_cmd::mongo_find_one_and_update,
            commands::mongo_cmd::mongo_find_one_and_replace,
            commands::mongo_cmd::mongo_find_one_and_delete,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_test_connection,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_tenants,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_tenant,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_tenant,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_update_tenant,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_tenant,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_namespaces,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_namespace,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_namespace,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_namespace_policies,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_topics,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_topic,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_topic,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_update_partitions,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_topic_stats,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_topic_internal_stats,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_exchanges,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_exchange,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_exchange,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_bindings,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_bind,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_unbind,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_subscriptions,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_subscription,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_subscription,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_skip_messages,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_reset_cursor,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_clear_backlog,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_consumer_group_config,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_alter_consumer_group_config,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_peek_messages,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_expire_messages,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_producers,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_consumers,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_unload_topic,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_client_connections,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_client_channels,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_close_client_connection,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_publish_rate,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_dispatch_rate,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_subscribe_rate,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_backlog_quota,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_retention,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_effective_policies,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_grant_permission,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_revoke_permission,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_permissions,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_users,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_create_user,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_user,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_user_permissions,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_grant_user_permission,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_revoke_user_permission,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_policies,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_set_policy,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_delete_policy,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_overview,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_nodes,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_issue_token,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_list_token_records,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_backlog,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_cluster_info,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_get_topic_route,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_alter_topic_config,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_skip_topic_accumulation,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_view_message,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_query_messages_by_key,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_query_messages_by_topic,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_query_message_trace,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_raw_request,
            #[cfg(feature = "mq-admin")]
            commands::mq_cmd::mq_send_message,
            commands::history::save_history,
            commands::history::load_history,
            commands::history::search_history,
            commands::history::load_history_connection_options,
            commands::history::clear_history,
            commands::history::delete_history_entry,
            commands::mcp::check_mcp_server_status,
            commands::mcp::install_mcp_server,
            commands::update::check_for_updates,
            commands::update::fetch_changelog,
            commands::update::get_system_proxy_url,
            commands::update::download_update,
            commands::update::cancel_update_download,
            commands::update::install_downloaded_update,
            commands::transfer::start_transfer,
            commands::transfer::preview_transfer_ownership,
            commands::transfer::cancel_transfer,
            commands::database_export::begin_database_backup_snapshot,
            commands::database_export::export_database_sql,
            commands::database_export::cancel_database_export,
            commands::table_export::start_table_export,
            commands::table_export::cancel_table_export,
            commands::query_result_export::start_query_result_export,
            commands::query_result_export::cancel_query_result_export,
            commands::csv_export::export_query_result_csv,
            commands::csv_export::export_table_data_csv,
            commands::xlsx_export::export_query_result_xlsx,
            commands::xlsx_export::export_query_results_xlsx,
            commands::text_export::export_query_result_json,
            commands::text_export::export_query_result_markdown,
            commands::agents::list_installed_agents,
            commands::agents::list_installed_agents_local,
            commands::agents::is_agent_installed,
            commands::agents::get_driver_store_usage,
            commands::agents::clear_driver_download_cache,
            commands::agents::get_driver_runtime_summary,
            commands::agents::stop_driver_runtime,
            commands::agents::restart_driver_runtime,
            commands::agents::install_agent,
            commands::agents::upgrade_all_agents,
            commands::agents::check_agent_update_blockers,
            commands::agents::uninstall_agent,
            commands::agents::check_jre_installed,
            commands::agents::get_agent_java_runtime_config,
            commands::agents::set_agent_java_runtime_config,
            commands::agents::uninstall_jre,
            commands::agents::reinstall_jre,
            commands::agents::invalidate_agent_registry_cache,
            commands::agents::import_agents_from_zip,
            commands::agents::import_agent_driver_cmd,
            commands::agents::import_agent_jar_cmd,
            commands::system_fonts::list_system_fonts,
            commands::ssh_config::list_ssh_config_hosts,
            commands::ssh_prompt::ssh_prompt_ready,
            commands::ssh_prompt::ssh_prompt_not_ready,
            commands::ssh_prompt::resolve_ssh_prompt,
            commands::tunnel_profiles::load_tunnel_profiles,
            commands::tunnel_profiles::save_tunnel_profiles,
            commands::tunnel_profiles::test_tunnel_profile,
        ]);
    append_startup_probe("building tauri application");
    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => {
            append_startup_probe(format!("tauri application built after {:?}", startup_begin.elapsed()));
            append_startup_probe(app_config_window_labels(&app));
            append_startup_probe(format!("post-build window probe: {}", main_window_probe_state(app.handle())));
            app
        }
        Err(error) => {
            append_startup_probe(startup_probe_build_error_message(&error.to_string()));
            panic!("error while building tauri application: {error}");
        }
    };
    append_startup_probe("entering tauri event loop");
    start_startup_probe_watchdog(app.handle());
    app.run(|app_handle, event| {
        log_startup_probe_run_event(app_handle, &event);

        #[cfg(not(target_os = "macos"))]
        let _ = (&app_handle, &event);

        if let RunEvent::ExitRequested { code, api, .. } = &event {
            let confirmed_exit =
                app_handle.try_state::<CloseBehaviorState>().map(|state| state.take_confirmed_exit()).unwrap_or(false);
            if should_confirm_app_exit_request(std::env::consts::OS, *code, confirmed_exit) {
                api.prevent_exit();
                request_app_close(app_handle, "quit");
            } else {
                tauri::async_runtime::block_on(async {
                    if let Some(server) = app_handle.try_state::<commands::redis_pubsub_server::PubSubServerState>() {
                        server.shutdown(Duration::from_secs(1)).await;
                    }
                    if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                        state.shutdown(Duration::from_secs(3)).await;
                    }
                });
            }
        }

        #[cfg(target_os = "macos")]
        if let RunEvent::Opened { urls } = &event {
            let links: Vec<String> = urls
                .iter()
                .map(|url| url.to_string())
                .filter_map(|url| commands::deep_link::connection_deep_link_from_arg(&url))
                .collect();
            open_connection_deep_links(app_handle, links);

            let paths: Vec<String> = urls
                .iter()
                .filter_map(|url| url.to_file_path().ok())
                .filter(|path| commands::external_sql::is_sql_file_path(path))
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            if !paths.is_empty() {
                if let Some(state) = app_handle.try_state::<commands::external_sql::ExternalSqlOpenState>() {
                    state.push(paths.clone());
                }
                let _ = app_handle.emit("dbx-open-sql-files", paths);
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            let db_paths: Vec<String> = urls
                .iter()
                .filter_map(|url| url.to_file_path().ok())
                .filter(|path| commands::external_db::is_db_file_path(path))
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            if !db_paths.is_empty() {
                if let Some(state) = app_handle.try_state::<commands::external_db::ExternalDbOpenState>() {
                    state.push(db_paths.clone());
                }
                let _ = app_handle.emit("dbx-open-db-files", db_paths);
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }

        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { has_visible_windows, .. } = &event {
            if !has_visible_windows {
                show_main_window(app_handle);
            }
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    state.refresh_connections().await;
                }
            });
        }

        if let RunEvent::Resumed = &event {
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    state.refresh_connections().await;
                }
            });
        }
    });
}
