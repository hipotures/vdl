//! Install and remove vdl's systemd user services and tmux attach helper.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{ensure_config, expand_tilde, load_config};

pub const OWNER_UNIT_NAME: &str = "vdl.service";
pub const WEB_UNIT_NAME: &str = "vdl-web.service";

pub fn systemd_user_dir() -> PathBuf {
    expand_tilde("~/.config/systemd/user")
}

pub fn tmux_config_path() -> PathBuf {
    expand_tilde("~/.config/vdl/tmux.conf")
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

pub fn owner_unit_text(application_command: &[String]) -> String {
    let mut cmd = application_command.to_vec();
    cmd.push("--owner-daemon".to_string());
    format!(
        "[Unit]\n\
Description=vdl download scheduler\n\
After=default.target\n\
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

pub fn web_unit_text(application_command: &[String]) -> String {
    let mut cmd = application_command.to_vec();
    cmd.push("--serve-web".to_string());
    format!(
        "[Unit]\n\
Description=vdl mobile web interface\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={}\n\
Restart=always\n\
RestartSec=10\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        exec_line(&cmd)
    )
}

pub fn write_tmux_config(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(
        path,
        "set-option -g exit-empty on\nset-option -g exit-unattached off\n",
    )
    .map_err(|e| e.to_string())
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
    let tmux_cfg = tmux_config_path();

    fs::create_dir_all(&unit_dir).map_err(|e| e.to_string())?;
    write_tmux_config(&tmux_cfg)?;

    fs::write(unit_dir.join(OWNER_UNIT_NAME), owner_unit_text(&app_cmd))
        .map_err(|e| e.to_string())?;
    fs::write(unit_dir.join(WEB_UNIT_NAME), web_unit_text(&app_cmd)).map_err(|e| e.to_string())?;

    run_systemctl(&["daemon-reload"], true)?;
    run_systemctl(&["enable", "--now", OWNER_UNIT_NAME, WEB_UNIT_NAME], true)?;

    let display_host = if config.web_host == "0.0.0.0" || config.web_host == "::" {
        "localhost"
    } else {
        config.web_host.as_str()
    };
    println!("Config: {}", config_path.display());
    println!("Owner status: systemctl --user status {OWNER_UNIT_NAME}");
    println!("Web status: systemctl --user status {WEB_UNIT_NAME}");
    println!("Attach: vdl attach");
    println!("Web: http://{display_host}:{}", config.web_port);
    Ok(())
}

pub fn deinstall_services() -> Result<(), String> {
    let unit_dir = systemd_user_dir();
    run_systemctl(&["disable", "--now", OWNER_UNIT_NAME, WEB_UNIT_NAME], false)?;
    for name in [OWNER_UNIT_NAME, WEB_UNIT_NAME] {
        let path = unit_dir.join(name);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.to_string());
        }
    }
    run_systemctl(&["daemon-reload"], true)?;
    println!("Removed vdl user services; configuration and downloads were preserved.");
    Ok(())
}

pub fn exec_attach(no_mouse: bool) -> Result<(), String> {
    let mut cmd = vdl_executable();
    cmd.push("--tui-client".to_string());
    let joined = exec_line(&cmd);

    let mut tmux = Command::new("tmux");
    tmux.args(["-L", "vdl"]);
    let tmux_cfg = tmux_config_path();
    if tmux_cfg.is_file() {
        tmux.arg("-f").arg(&tmux_cfg);
    }
    tmux.args(["new-session", "-A", "-s", "main", &joined]);
    if no_mouse {
        tmux.env("VDL_MOUSE", "0");
    }
    let err = tmux.exec();
    Err(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_expected_systemd_units() {
        let cmd = vec!["/usr/local/bin/vdl".to_string()];
        let owner = owner_unit_text(&cmd);
        assert!(owner.contains("ExecStart=/usr/local/bin/vdl --owner-daemon"));
        assert!(owner.contains("KillMode=control-group"));

        let web = web_unit_text(&cmd);
        assert!(web.contains("ExecStart=/usr/local/bin/vdl --serve-web"));
        assert!(web.contains("After=network-online.target"));
    }
}
