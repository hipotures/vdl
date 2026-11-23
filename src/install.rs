//! Install and remove vdl's single systemd user service (`vdl.service`).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::{ensure_config, expand_tilde, load_config};

pub const SERVICE_UNIT_NAME: &str = "vdl.service";
const LEGACY_WEB_UNIT_NAME: &str = "vdl-web.service";

pub fn systemd_user_dir() -> PathBuf {
    expand_tilde("~/.config/systemd/user")
}

pub fn vdl_executable() -> Vec<String> {
    if let Ok(exe) = std::env::current_exe()
        && let Ok(canon) = fs::canonicalize(&exe)
    {
        return vec![canon.to_string_lossy().into_owned()];
    }
    vec!["vdl".to_string()]
}

fn shlex_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-' | b'_'
                )
        })
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn exec_line(args: &[String]) -> String {
    args.iter()
        .map(|s| shlex_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn service_unit_text(application_command: &[String]) -> String {
    let mut cmd = application_command.to_vec();
    cmd.push("serve".to_string());
    format!(
        "[Unit]\n\
Description=vdl download server and web interface\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={}\n\
KillMode=control-group\n\
Restart=always\n\
RestartSec=10\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        exec_line(&cmd)
    )
}

fn run_systemctl(args: &[&str], check: bool) -> Result<(), String> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    if check && !status.success() {
        return Err(format!("systemctl --user {} failed", args.join(" ")));
    }
    Ok(())
}

pub fn install_services() -> Result<(), String> {
    let config_path = ensure_config(None).map_err(|e| e.to_string())?;
    let config = load_config(Some(&config_path)).map_err(|e| e.to_string())?;
    let app_cmd = vdl_executable();
    let unit_dir = systemd_user_dir();

    fs::create_dir_all(&unit_dir).map_err(|e| e.to_string())?;

    // Clean up obsolete split web service if present.
    let legacy_web = unit_dir.join(LEGACY_WEB_UNIT_NAME);
    if legacy_web.exists() {
        let _ = run_systemctl(&["disable", "--now", LEGACY_WEB_UNIT_NAME], false);
        let _ = fs::remove_file(&legacy_web);
    }

    fs::write(
        unit_dir.join(SERVICE_UNIT_NAME),
        service_unit_text(&app_cmd),
    )
    .map_err(|e| e.to_string())?;

    run_systemctl(&["daemon-reload"], true)?;
    run_systemctl(&["enable", "--now", SERVICE_UNIT_NAME], true)?;

    let display_host = if config.web_host == "0.0.0.0" || config.web_host == "::" {
        "localhost"
    } else {
        config.web_host.as_str()
    };
    println!("Config: {}", config_path.display());
    println!("Service status: systemctl --user status {SERVICE_UNIT_NAME}");
    println!("TUI: vdl");
    println!("Web: http://{display_host}:{}", config.web_port);
    Ok(())
}

pub fn deinstall_services() -> Result<(), String> {
    let unit_dir = systemd_user_dir();
    run_systemctl(
        &["disable", "--now", SERVICE_UNIT_NAME, LEGACY_WEB_UNIT_NAME],
        false,
    )?;
    for name in [SERVICE_UNIT_NAME, LEGACY_WEB_UNIT_NAME] {
        let path = unit_dir.join(name);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.to_string());
        }
    }
    run_systemctl(&["daemon-reload"], true)?;
    println!("Removed vdl user service; configuration and downloads were preserved.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_expected_systemd_unit() {
        let cmd = vec!["/usr/local/bin/vdl".to_string()];
        let unit = service_unit_text(&cmd);
        assert!(unit.contains("ExecStart=/usr/local/bin/vdl serve"));
        assert!(unit.contains("KillMode=control-group"));
        assert!(unit.contains("After=network-online.target"));
    }
}
