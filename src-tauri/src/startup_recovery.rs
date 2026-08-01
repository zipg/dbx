use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::Manager;

const STARTUP_LOG_FILE: &str = "startup.log";
const STARTUP_LOG_DIR_ENV: &str = "DBX_STARTUP_LOG_DIR";
const KEEP_STARTUP_LOG_ENV: &str = "DBX_KEEP_STARTUP_LOG";
#[cfg(target_os = "windows")]
const NO_SANDBOX_ENV: &str = "DBX_WEBVIEW2_NO_SANDBOX";
const RECOVERY_ATTEMPT_ENV: &str = "DBX_STARTUP_COMPAT_RECOVERY";
const DISABLE_ENTERPRISE_COMPAT_ENV: &str = "DBX_DISABLE_ENTERPRISE_COMPAT";
const WINDOWS_APP_DATA_DIR_NAME: &str = "com.dbx.app";
const COMPATIBILITY_MARKER_FILE: &str = "webview2-enterprise-compat.enabled";
#[cfg(any(target_os = "windows", test))]
const COMPATIBILITY_PROFILE_DIR: &str = "webview2-enterprise-compat";
const STARTUP_LOG_BUFFER_CAPACITY: usize = 256;
const STARTUP_WATCHDOG_DELAY: Duration = Duration::from_secs(15);

#[derive(Default)]
struct StartupProbeState {
    active: bool,
    persistent: bool,
    recovery_attempt: bool,
    enterprise_compat: bool,
    run_event_count: usize,
    lines: VecDeque<String>,
}

static STARTUP_PROBE_STATE: LazyLock<Mutex<StartupProbeState>> =
    LazyLock::new(|| Mutex::new(StartupProbeState::default()));

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1"))
}

fn startup_log_dir_from_inputs(
    target_os: &str,
    explicit_dir: Option<OsString>,
    windows_appdata: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = explicit_dir.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    (target_os == "windows")
        .then(|| windows_appdata.filter(|value| !value.is_empty()))
        .flatten()
        .map(PathBuf::from)
        .map(|dir| dir.join(WINDOWS_APP_DATA_DIR_NAME))
}

fn startup_log_dir() -> Option<PathBuf> {
    startup_log_dir_from_inputs(
        std::env::consts::OS,
        std::env::var_os(STARTUP_LOG_DIR_ENV),
        std::env::var_os("APPDATA"),
    )
}

fn startup_log_path() -> Option<PathBuf> {
    startup_log_dir().map(|dir| dir.join(STARTUP_LOG_FILE))
}

fn compatibility_marker_path_from_appdata(windows_appdata: Option<OsString>) -> Option<PathBuf> {
    windows_appdata
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join(WINDOWS_APP_DATA_DIR_NAME).join(COMPATIBILITY_MARKER_FILE))
}

fn compatibility_marker_path() -> Option<PathBuf> {
    compatibility_marker_path_from_appdata(std::env::var_os("APPDATA"))
}

#[cfg(any(target_os = "windows", test))]
fn compatibility_profile_path_from_local_appdata(local_appdata: Option<OsString>) -> Option<PathBuf> {
    local_appdata
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join(WINDOWS_APP_DATA_DIR_NAME).join(COMPATIBILITY_PROFILE_DIR))
}

#[cfg(target_os = "windows")]
fn compatibility_profile_path() -> Option<PathBuf> {
    compatibility_profile_path_from_local_appdata(
        std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")),
    )
}

fn ensure_parent_dir(path: &Path) -> bool {
    path.parent().is_some_and(|dir| std::fs::create_dir_all(dir).is_ok())
}

fn write_line(path: &Path, line: &str) {
    if !ensure_parent_dir(path) {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn format_probe_line(message: &str) -> String {
    format!("[{}][pid={}] {message}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), std::process::id())
}

pub(crate) fn initialize() {
    let recovery_attempt = cfg!(target_os = "windows") && env_flag(RECOVERY_ATTEMPT_ENV);
    let marker_enabled = cfg!(target_os = "windows")
        && !env_flag(DISABLE_ENTERPRISE_COMPAT_ENV)
        && compatibility_marker_path().is_some_and(|path| path.is_file());
    let enterprise_compat = recovery_attempt || marker_enabled;

    if !recovery_attempt {
        if let Some(path) = startup_log_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    {
        let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = StartupProbeState {
            active: true,
            persistent: recovery_attempt,
            recovery_attempt,
            enterprise_compat,
            run_event_count: 0,
            lines: VecDeque::with_capacity(STARTUP_LOG_BUFFER_CAPACITY),
        };
    }

    install_panic_hook();
    record(format!(
        "process start version={} os={} arch={} recovery_attempt={} compatibility_marker={} enterprise_compat={}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        recovery_attempt,
        marker_enabled,
        enterprise_compat
    ));
    configure_webview2_compatibility(enterprise_compat);
}

pub(crate) fn record(message: impl AsRef<str>) {
    let line = format_probe_line(message.as_ref());
    let persistent = {
        let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active {
            return;
        }
        if state.lines.len() == STARTUP_LOG_BUFFER_CAPACITY {
            state.lines.pop_front();
        }
        state.lines.push_back(line.clone());
        state.persistent
    };
    if persistent {
        if let Some(path) = startup_log_path() {
            write_line(&path, &line);
        }
    }
}

fn persist_buffer() {
    let lines = {
        let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.persistent {
            return;
        }
        state.persistent = true;
        state.lines.iter().cloned().collect::<Vec<_>>()
    };
    let Some(path) = startup_log_path() else {
        return;
    };
    if !ensure_parent_dir(&path) {
        return;
    }
    let Ok(mut file) = std::fs::File::create(path) else {
        return;
    };
    for line in lines {
        let _ = writeln!(file, "{line}");
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!(" line={} column={}", location.line(), location.column()))
            .unwrap_or_default();
        record(format!("panic before frontend ready{location}"));
        persist_buffer();
        default_hook(info);
    }));
}

#[cfg(target_os = "windows")]
fn append_webview2_argument(argument: &str) {
    let mut args = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
    if args.split_whitespace().any(|value| value == argument) {
        return;
    }
    if !args.is_empty() {
        args.push(' ');
    }
    args.push_str(argument);
    std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", args);
}

#[cfg(target_os = "windows")]
fn configure_webview2_compatibility(enterprise_compat: bool) {
    let manual_no_sandbox = env_flag(NO_SANDBOX_ENV);
    if enterprise_compat {
        match compatibility_profile_path() {
            Some(path) => match std::fs::create_dir_all(&path) {
                Ok(()) => {
                    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &path);
                    record(format!("enterprise compatibility profile={}", path.display()));
                }
                Err(error) => record(format!("failed to create enterprise compatibility profile: {error}")),
            },
            None => record("enterprise compatibility profile unavailable: LOCALAPPDATA and APPDATA are missing"),
        }
    }
    if enterprise_compat || manual_no_sandbox {
        append_webview2_argument("--no-sandbox");
        record(format!("WebView2 no-sandbox enabled enterprise_compat={enterprise_compat} manual={manual_no_sandbox}"));
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_webview2_compatibility(_enterprise_compat: bool) {}

pub(crate) fn record_run_event() {
    let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.active {
        state.run_event_count = state.run_event_count.saturating_add(1);
    }
}

pub(crate) fn is_recovery_attempt() -> bool {
    STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).recovery_attempt
}

fn probe_snapshot() -> (bool, bool, usize) {
    let state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    (state.active, state.enterprise_compat, state.run_event_count)
}

fn should_attempt_enterprise_recovery(
    active: bool,
    enterprise_compat: bool,
    run_event_count: usize,
    main_exists: bool,
) -> bool {
    active && !enterprise_compat && run_event_count == 0 && !main_exists
}

pub(crate) fn start_watchdog<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if std::env::consts::OS != "windows" {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(STARTUP_WATCHDOG_DELAY);
        let (active, enterprise_compat, run_event_count) = probe_snapshot();
        if !active {
            return;
        }
        let main_exists = app.get_webview_window("main").is_some();
        record(format!(
            "startup watchdog after {}s run_event_count={run_event_count} main_exists={main_exists}",
            STARTUP_WATCHDOG_DELAY.as_secs()
        ));
        if should_attempt_enterprise_recovery(active, enterprise_compat, run_event_count, main_exists) {
            persist_buffer();
            record("startup stalled before event loop; restarting once with enterprise compatibility mode");
            let restart_result = std::env::current_exe().and_then(|executable| {
                std::process::Command::new(executable)
                    .args(std::env::args_os().skip(1))
                    .env(RECOVERY_ATTEMPT_ENV, "1")
                    .spawn()
            });
            match restart_result {
                Ok(child) => {
                    record(format!("enterprise compatibility process spawned pid={}", child.id()));
                    std::process::exit(0);
                }
                Err(error) => {
                    record(format!("failed to spawn enterprise compatibility process: {error}"));
                    show_recovery_failure_message();
                    return;
                }
            }
        }
        if run_event_count > 0 || main_exists {
            return;
        }

        persist_buffer();
        if enterprise_compat {
            record("enterprise compatibility startup failed; automatic recovery stopped");
            show_recovery_failure_message();
            std::process::exit(1);
        }
    });
}

fn write_compatibility_marker() -> Result<PathBuf, String> {
    let path = compatibility_marker_path().ok_or_else(|| "APPDATA is unavailable".to_string())?;
    if !ensure_parent_dir(&path) {
        return Err("failed to create compatibility marker directory".to_string());
    }
    std::fs::write(&path, "mode=isolated-profile-no-sandbox\n").map_err(|error| error.to_string())?;
    Ok(path)
}

pub(crate) fn mark_frontend_ready() {
    let (recovery_attempt, keep_requested) = {
        let state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.recovery_attempt, env_flag(KEEP_STARTUP_LOG_ENV))
    };

    if recovery_attempt {
        match write_compatibility_marker() {
            Ok(path) => record(format!("enterprise compatibility recovery succeeded marker={}", path.display())),
            Err(error) => record(format!("enterprise compatibility recovery succeeded but marker failed: {error}")),
        }
        persist_buffer();
        show_recovery_success_message();
        std::env::remove_var(RECOVERY_ATTEMPT_ENV);
    } else if keep_requested {
        record("frontend ready; startup log retained by DBX_KEEP_STARTUP_LOG=1");
        persist_buffer();
    } else if let Some(path) = startup_log_path() {
        let _ = std::fs::remove_file(path);
    }

    let mut state = STARTUP_PROBE_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.active = false;
    state.lines.clear();
}

#[cfg(target_os = "windows")]
fn windows_message_box(title: &str, body: &str, warning: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
    };

    let title = title.encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
    let body = body.encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
    let icon = if warning { MB_ICONWARNING } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | icon | MB_SETFOREGROUND);
    }
}

#[cfg(target_os = "windows")]
fn show_recovery_success_message() {
    let log_path =
        startup_log_path().map(|path| path.display().to_string()).unwrap_or_else(|| "startup.log".to_string());
    let locale = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
    let body = if locale.starts_with("zh") {
        format!(
            "DBX 已自动启用企业环境兼容模式，主界面已恢复。\n\n后续启动会直接使用该模式，无需再次等待。\n本次恢复日志：{log_path}"
        )
    } else {
        format!(
            "DBX automatically enabled enterprise environment compatibility mode and restored the main window.\n\nFuture launches will use this mode directly.\nRecovery log: {log_path}"
        )
    };
    windows_message_box("DBX", &body, false);
}

#[cfg(not(target_os = "windows"))]
fn show_recovery_success_message() {}

#[cfg(target_os = "windows")]
fn show_recovery_failure_message() {
    let log_path =
        startup_log_path().map(|path| path.display().to_string()).unwrap_or_else(|| "startup.log".to_string());
    let locale = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
    let body = if locale.starts_with("zh") {
        format!("DBX 已尝试企业环境兼容模式，但主窗口仍未创建。\n\n请将此日志发给维护者：{log_path}")
    } else {
        format!(
            "DBX tried enterprise environment compatibility mode, but the main window was still not created.\n\nPlease send this log to the maintainer: {log_path}"
        )
    };
    windows_message_box("DBX", &body, true);
}

#[cfg(not(target_os = "windows"))]
fn show_recovery_failure_message() {}

#[cfg(test)]
mod tests {
    use super::{
        compatibility_marker_path_from_appdata, compatibility_profile_path_from_local_appdata,
        should_attempt_enterprise_recovery, startup_log_dir_from_inputs,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn startup_log_uses_windows_appdata() {
        assert_eq!(
            startup_log_dir_from_inputs("windows", None, Some(OsString::from(r"C:\Users\test\AppData\Roaming")),),
            Some(PathBuf::from(r"C:\Users\test\AppData\Roaming").join("com.dbx.app"))
        );
    }

    #[test]
    fn startup_log_prefers_explicit_directory() {
        assert_eq!(
            startup_log_dir_from_inputs(
                "windows",
                Some(OsString::from(r"D:\DBXDiagnostics")),
                Some(OsString::from(r"C:\Users\test\AppData\Roaming")),
            ),
            Some(PathBuf::from(r"D:\DBXDiagnostics"))
        );
    }

    #[test]
    fn enterprise_compatibility_paths_are_isolated() {
        assert_eq!(
            compatibility_marker_path_from_appdata(Some(OsString::from(r"C:\Users\test\AppData\Roaming"))),
            Some(
                PathBuf::from(r"C:\Users\test\AppData\Roaming")
                    .join("com.dbx.app")
                    .join("webview2-enterprise-compat.enabled")
            )
        );
        assert_eq!(
            compatibility_profile_path_from_local_appdata(Some(OsString::from(r"C:\Users\test\AppData\Local"))),
            Some(PathBuf::from(r"C:\Users\test\AppData\Local").join("com.dbx.app").join("webview2-enterprise-compat"))
        );
    }

    #[test]
    fn recovery_only_triggers_for_the_observed_hard_startup_stall() {
        assert!(should_attempt_enterprise_recovery(true, false, 0, false));
        assert!(!should_attempt_enterprise_recovery(false, false, 0, false));
        assert!(!should_attempt_enterprise_recovery(true, true, 0, false));
        assert!(!should_attempt_enterprise_recovery(true, false, 1, false));
        assert!(!should_attempt_enterprise_recovery(true, false, 0, true));
    }
}
