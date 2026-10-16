use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use castor_node::conf::Conf;
use castor_node::sys::{GATE_PATH, ROOT_PATH, Real};
use castor_node::{Fail, Result, gate, snap, zfs};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = run(&args);
    let _ = std::io::stdout().flush();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            if !f.msg.is_empty() {
                eprintln!("{}", f.msg);
            }
            ExitCode::from(u8::try_from(f.code).unwrap_or(1))
        }
    }
}

fn run(args: &[String]) -> Result {
    let exe = std::env::current_exe().map_err(|e| Fail::new(1, format!("castor-node: {e}")))?;
    let bin = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let invoked = args.first().map(|a| {
        Path::new(a)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    // Installed as castor-ssh / castor-zfs / castor-snap, or `castor-node <tool> ...`.
    let (tool, rest): (String, Vec<&str>) = match invoked.as_deref() {
        Some(t @ ("castor-ssh" | "castor-zfs" | "castor-snap")) => {
            (t.to_owned(), args[1..].iter().map(String::as_str).collect())
        }
        _ => match args.get(1) {
            Some(t) => (t.clone(), args[2..].iter().map(String::as_str).collect()),
            None => {
                return Err(Fail::invalid(
                    "usage: castor-node <castor-ssh|castor-zfs|castor-snap> ...",
                ));
            }
        },
    };
    let conf = Conf::load(&bin.join("castor.conf"), true)?;
    let mut stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();

    match tool.as_str() {
        "castor-ssh" => {
            let mut sys = Real { path: GATE_PATH };
            let home = std::env::var("HOME").ok();
            let command = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
            let mut c = gate::Ctx {
                sys: &mut sys,
                conf: conf.gate(home.as_deref())?,
                bin,
                stdin: &mut stdin,
                out: &mut out,
            };
            gate::main(&mut c, &command)
        }
        "castor-zfs" => {
            let mut sys = Real { path: ROOT_PATH };
            let darwin = cfg!(target_os = "macos");
            let mut c = zfs::Ctx {
                sys: &mut sys,
                layout: conf.layout()?,
                stdin: &mut stdin,
                out: &mut out,
                darwin,
            };
            zfs::main(&mut c, &rest)
        }
        "castor-snap" => {
            let mut sys = Real { path: ROOT_PATH };
            let mut c = snap::Ctx {
                sys: &mut sys,
                layout: conf.layout()?,
                out: &mut out,
            };
            snap::main(&mut c, &rest)
        }
        other => Err(Fail::invalid(format!("castor-node: unknown tool {other}"))),
    }
}
