//! 현재 설정을 읽어(비밀 값은 지우고) JSON 으로 출력한다. 개발·진단용.
//!
//!     cargo run -p worklog-core --example dump_config
//!     WORKLOG_SETTINGS=path\to\settings.json cargo run -p worklog-core --example dump_config

fn main() {
    let cfg = worklog_core::config::Config::load();
    let presence = cfg.secrets_presence();
    let redacted = cfg.redacted();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "settings_path": worklog_core::paths::settings_path(),
            "secrets_present": presence,
            "config": redacted,
        }))
        .expect("json")
    );
}
