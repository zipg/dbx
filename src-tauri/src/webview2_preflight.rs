//! TEMPORARY STUB — replaced by the preflight-impl teammate.
//!
//! Windows WebView2 startup diagnostics and preflight probes. See task-1.

use std::path::Path;
use std::time::Duration;

pub(crate) const PREFLIGHT_CHILD_ENV: &str = "DBX_WEBVIEW2_PREFLIGHT_CHILD";
pub(crate) const PREFLIGHT_OUT_ENV: &str = "DBX_WEBVIEW2_PREFLIGHT_OUT";
pub(crate) const PREFLIGHT_TIMEOUT_MS_ENV: &str = "DBX_WEBVIEW2_PREFLIGHT_TIMEOUT_MS";

pub(crate) struct Diagnostics;

pub(crate) fn collect_diagnostics() -> Diagnostics {
    Diagnostics
}

pub(crate) fn format_diagnostics(_d: &Diagnostics) -> Vec<String> {
    vec!["webview2 diagnostics unavailable (stub)".to_string()]
}

pub(crate) struct EnvironmentProbe {
    pub ok: bool,
    pub hresult: Option<String>,
    pub message: Option<String>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
}

pub(crate) fn probe_environment(
    _additional_args: Option<&str>,
    _user_data_folder: Option<&Path>,
    _timeout: Duration,
) -> EnvironmentProbe {
    EnvironmentProbe { ok: false, hresult: None, message: None, elapsed_ms: 0, timed_out: false }
}

pub(crate) fn run_child_if_requested() -> bool {
    false
}

pub(crate) struct PreflightChildOutcome {
    pub spawned: bool,
    pub ok: bool,
    pub hresult: Option<String>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
}

pub(crate) fn spawn_preflight_child(_timeout: Duration) -> PreflightChildOutcome {
    PreflightChildOutcome { spawned: false, ok: false, hresult: None, elapsed_ms: 0, timed_out: false, exit_code: None }
}
