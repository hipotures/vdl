mod client;
mod config;
mod domain;
mod health;
mod install;
mod repository;
mod runner;
mod scheduler;
mod state;
mod ui;
mod web;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crate::client::VdlClient;
use crate::config::load_config;
use crate::domain::{format_snapshots_table, resolve_snapshot_numbers};
use crate::install::{deinstall_services, install_services};

const HELP_TEXT: &str = "\
usage: vdl [-h] [--server URL] [--no-mouse] {tui,serve,list,add,del,now,install,deinstall} ...

positional arguments:
  {tui,serve,list,add,del,now,install,deinstall}
    tui                 open the terminal UI client (default)
    serve               run the headless vdl server (scheduler + HTTP API + web UI)
    list                list sources from the server
    add                 add a source via the server
    del                 disable sources via the server
    now                 queue immediate download for sources via the server
    install             install systemd user service (vdl.service) and configuration
    deinstall           remove vdl systemd user service

options:
  -h, --help            show this help message and exit
  --server URL          server base URL (overrides VDL_SERVER and config server_url)
  --no-mouse            start the TUI without mouse capture (F2 toggles it)
";

fn run_del(client: &VdlClient, ids: &[String]) -> Result<u8, String> {
    let snap = client.get_state()?;
    let selected = resolve_snapshot_numbers(&snap.sources, ids).map_err(|e| e.to_string())?;
    let targets: Vec<_> = selected.into_iter().filter(|(_, src)| src.active).collect();
    if targets.is_empty() {
        println!("The selected sources are already inactive.");
        return Ok(0);
    }
    let nums: Vec<usize> = targets.iter().map(|(n, _)| *n).collect();
    let srcs: Vec<_> = targets.iter().map(|(_, s)| s.clone()).collect();
    println!("Sources to disable:");
    println!("{}", format_snapshots_table(&srcs, Some(&nums)));
    print!("Disable these sources? [y/N] ");
    let _ = io::stdout().flush();

    let mut line = String::new();
    let read = io::stdin().read_line(&mut line).unwrap_or(0);
    let answer = if read == 0 {
        String::new()
    } else {
        line.trim().to_lowercase()
    };
    if answer != "y" && answer != "yes" {
        println!("No changes made.");
        return Ok(1);
    }
    for (_, src) in &targets {
        client.apply_action("disable", &src.id)?;
    }
    println!("Disabled {} source(s).", targets.len());
    Ok(0)
}

fn run_now(client: &VdlClient, ids: &[String]) -> Result<u8, String> {
    let snap = client.get_state()?;
    let selected = resolve_snapshot_numbers(&snap.sources, ids).map_err(|e| e.to_string())?;
    for (_, src) in &selected {
        client.apply_action("download-now", &src.id)?;
        println!("Queued: {}", src.id);
    }
    Ok(0)
}

fn dispatch_cli(args: &[String]) -> ExitCode {
    let mut no_mouse = false;
    let mut server_override: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" if positional.is_empty() => {
                print!("{HELP_TEXT}");
                return ExitCode::SUCCESS;
            }
            "--no-mouse" => {
                no_mouse = true;
                i += 1;
            }
            "--server" => {
                if i + 1 >= args.len() {
                    eprintln!("vdl: --server requires a URL argument");
                    return ExitCode::from(2);
                }
                server_override = Some(args[i + 1].clone());
                i += 2;
            }
            arg if arg.starts_with("--server=") => {
                server_override = Some(arg["--server=".len()..].to_string());
                i += 1;
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let result: Result<u8, String> = if positional.is_empty() || positional[0] == "tui" {
        load_config(None)
            .map_err(|e| e.to_string())
            .and_then(|cfg| {
                let url = cfg.resolved_server_url(server_override.as_deref());
                ui::run_tui(url, if no_mouse { Some(false) } else { None })
            })
            .map(|()| 0)
    } else {
        match positional[0].as_str() {
            "serve" => {
                let mut config_path: Option<PathBuf> = None;
                let mut j = 1;
                while j < positional.len() {
                    match positional[j].as_str() {
                        "--config" => {
                            if j + 1 >= positional.len() {
                                eprintln!("vdl serve: --config requires a path");
                                return ExitCode::from(2);
                            }
                            config_path = Some(PathBuf::from(&positional[j + 1]));
                            j += 2;
                        }
                        arg if arg.starts_with("--config=") => {
                            config_path = Some(PathBuf::from(&arg["--config=".len()..]));
                            j += 1;
                        }
                        "-h" | "--help" => {
                            println!("usage: vdl serve [--config PATH]");
                            return ExitCode::SUCCESS;
                        }
                        bad => {
                            eprintln!("vdl serve: unrecognized argument: {bad}");
                            return ExitCode::from(2);
                        }
                    }
                }
                load_config(config_path.as_deref())
                    .map_err(|e| e.to_string())
                    .and_then(web::serve)
                    .map(|()| 0)
            }
            "install" => install_services().map(|()| 0),
            "deinstall" => deinstall_services().map(|()| 0),
            "list" => load_config(None)
                .map_err(|e| e.to_string())
                .and_then(|cfg| {
                    let client =
                        VdlClient::new(cfg.resolved_server_url(server_override.as_deref()))?;
                    let snap = client.get_state()?;
                    println!("{}", format_snapshots_table(&snap.sources, None));
                    Ok(0)
                }),
            "add" => {
                if positional.len() != 2 {
                    eprintln!("usage: vdl add <url>");
                    return ExitCode::from(2);
                }
                load_config(None)
                    .map_err(|e| e.to_string())
                    .and_then(|cfg| {
                        let client =
                            VdlClient::new(cfg.resolved_server_url(server_override.as_deref()))?;
                        let res = client.add_source(&positional[1])?;
                        let verb = if res.created {
                            "Added"
                        } else {
                            "Already exists"
                        };
                        println!("{verb}: {}", res.id);
                        Ok(0)
                    })
            }
            "del" => {
                if positional.len() < 2 {
                    eprintln!("usage: vdl del <ids...>");
                    return ExitCode::from(2);
                }
                load_config(None)
                    .map_err(|e| e.to_string())
                    .and_then(|cfg| {
                        let client =
                            VdlClient::new(cfg.resolved_server_url(server_override.as_deref()))?;
                        run_del(&client, &positional[1..])
                    })
            }
            "now" => {
                if positional.len() < 2 {
                    eprintln!("usage: vdl now <ids...>");
                    return ExitCode::from(2);
                }
                load_config(None)
                    .map_err(|e| e.to_string())
                    .and_then(|cfg| {
                        let client =
                            VdlClient::new(cfg.resolved_server_url(server_override.as_deref()))?;
                        run_now(&client, &positional[1..])
                    })
            }
            unknown => {
                eprintln!(
                    "vdl: error: invalid command '{unknown}' (choose from tui, serve, list, add, del, now, install, deinstall)"
                );
                return ExitCode::from(2);
            }
        }
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("vdl: {err}");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch_cli(&args)
}
