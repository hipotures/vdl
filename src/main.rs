mod config;
mod domain;
mod install;
mod owner;
mod repository;
mod runner;
mod runtime;
mod scheduler;
mod state;
mod ui;
mod web;

use std::io::{self, Write};
use std::process::ExitCode;

use crate::config::load_config;
use crate::domain::{disable_sources, format_sources_table, resolve_source_numbers};
use crate::install::{deinstall_services, exec_attach, install_services};
use crate::repository::SourceRepository;

const HELP_TEXT: &str = "\
usage: vdl [-h] [--no-mouse] {attach,list,add,del,install,deinstall} ...

positional arguments:
  {attach,list,add,del,install,deinstall}
    attach              open or attach to the persistent Rich TUI
    list                list sources
    add                 add a source
    del                 disable sources
    install             install user services and configuration
    deinstall           remove vdl user services

options:
  -h, --help            show this help message and exit
  --no-mouse            start the terminal without mouse capture (F2 toggles
                        it)
";

fn run_del(repository: &SourceRepository, ids: &[String]) -> Result<u8, String> {
    let sources = repository.list_sources();
    let selected = resolve_source_numbers(&sources, ids).map_err(|e| e.to_string())?;
    let targets: Vec<_> = selected.into_iter().filter(|s| s.active).collect();
    if targets.is_empty() {
        println!("The selected sources are already inactive.");
        return Ok(0);
    }
    let target_numbers: Vec<usize> = targets
        .iter()
        .filter_map(|target| sources.iter().position(|s| s == target).map(|i| i + 1))
        .collect();
    println!("Sources to disable:");
    println!("{}", format_sources_table(&targets, Some(&target_numbers)));
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
    let disabled = disable_sources(repository, &targets).map_err(|e| e.to_string())?;
    println!("Disabled {} source(s).", disabled.len());
    Ok(0)
}

fn dispatch_cli(args: &[String]) -> ExitCode {
    if args == ["--serve-web"] {
        return match load_config(None)
            .map_err(|e| e.to_string())
            .and_then(web::serve)
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("vdl: {err}");
                ExitCode::from(2)
            }
        };
    }

    if args == ["--owner-daemon"] {
        return match load_config(None)
            .map_err(|e| e.to_string())
            .and_then(owner::run_owner)
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("vdl: {err}");
                ExitCode::from(2)
            }
        };
    }

    if args == ["--tui-client"] {
        return match load_config(None)
            .map_err(|e| e.to_string())
            .and_then(|cfg| ui::run_tui(cfg, None))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("vdl: {err}");
                ExitCode::from(2)
            }
        };
    }

    // Parse flags before subcommand
    let mut no_mouse = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-h" | "--help" => {
                print!("{HELP_TEXT}");
                return ExitCode::SUCCESS;
            }
            "--no-mouse" => {
                no_mouse = true;
                idx += 1;
            }
            arg if arg.starts_with('-') => {
                eprintln!(
                    "usage: vdl [-h] [--no-mouse] {{attach,list,add,del,install,deinstall}} ..."
                );
                eprintln!("vdl: error: unrecognized arguments: {arg}");
                return ExitCode::from(2);
            }
            _ => break,
        }
    }

    let rest = &args[idx..];
    let result: Result<u8, String> = if rest.is_empty() {
        load_config(None)
            .map_err(|e| e.to_string())
            .and_then(|cfg| ui::run_tui(cfg, if no_mouse { Some(false) } else { None }))
            .map(|()| 0)
    } else {
        match rest[0].as_str() {
            "attach" => exec_attach(no_mouse).map(|()| 0),
            "install" => install_services().map(|()| 0),
            "deinstall" => deinstall_services().map(|()| 0),
            "list" => load_config(None)
                .map_err(|e| e.to_string())
                .and_then(|cfg| {
                    let repo =
                        SourceRepository::new(&cfg.download_root).map_err(|e| e.to_string())?;
                    println!("{}", format_sources_table(&repo.list_sources(), None));
                    Ok(0)
                }),
            "add" => {
                if rest.len() != 2 {
                    eprintln!("usage: vdl add [-h] url");
                    eprintln!("vdl add: error: the following arguments are required: url");
                    return ExitCode::from(2);
                }
                load_config(None)
                    .map_err(|e| e.to_string())
                    .and_then(|cfg| {
                        let repo =
                            SourceRepository::new(&cfg.download_root).map_err(|e| e.to_string())?;
                        let res = repo.add_source(&rest[1]).map_err(|e| e.to_string())?;
                        let verb = if res.created {
                            "Added"
                        } else {
                            "Already exists"
                        };
                        println!("{verb}: {}/{}", res.source.service, res.source.account);
                        Ok(0)
                    })
            }
            "del" => {
                if rest.len() < 2 {
                    eprintln!("usage: vdl del [-h] ids [ids ...]");
                    eprintln!("vdl del: error: the following arguments are required: ids");
                    return ExitCode::from(2);
                }
                load_config(None)
                    .map_err(|e| e.to_string())
                    .and_then(|cfg| {
                        let repo =
                            SourceRepository::new(&cfg.download_root).map_err(|e| e.to_string())?;
                        run_del(&repo, &rest[1..])
                    })
            }
            unknown => {
                eprintln!(
                    "usage: vdl [-h] [--no-mouse] {{attach,list,add,del,install,deinstall}} ..."
                );
                eprintln!(
                    "vdl: error: argument command: invalid choice: '{unknown}' (choose from attach, list, add, del, install, deinstall)"
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
