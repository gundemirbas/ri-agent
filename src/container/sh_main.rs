//! `ri-sh` — the sandbox's static POSIX shell.
//!
//! A thin, minimal `sh` CLI built on the embeddable [`epsh`] shell library
//! (Rust, no external binaries). Every script is parsed and executed by the
//! EPSH interpreter in-process; external commands resolve through the sandbox
//! `PATH` and are launched by EPSH. Because `ri-sh` is a static musl binary it
//! runs in the minimal sandbox image with **zero** dynamic libraries — the
//! image keeps only static binaries (ri-sh + static coreutils + static custom
//! tools), so the host's `/bin/sh` copy and `/lib` binds are dropped entirely
//! in strict mode (see `docs/CONTAINER-RUNTIME-SPEC.md` §15).
//!
//! Supported invocation (POSIX `sh` subset used by the `bash` tool):
//! - `ri-sh -c <script> [arg…]` — run a command string; rest are `$0…`
//! - `ri-sh <script-file> [arg…]` — run a script file
//! - `ri-sh -s` / `ri-sh` — read the script from stdin
//! - options `-e` (errexit), `-u` (nounset), `-x` (xtrace), `-f` (noglob)

use std::ffi::OsString;
use std::io::Read;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let mut shell = epsh::eval::Shell::new();

    // Mirror the tiny POSIX-option loop; `-o option` long flags are accepted
    // but ri-sh only implements the handful the bash tool needs.
    let mut i = 1usize;
    let mut from_stdin = false;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        if arg == "--" {
            i += 1;
            break;
        }
        if arg == "-c" {
            let Some(script) = args.get(i + 1) else {
                eprintln!("ri-sh: -c requires an argument");
                std::process::exit(2);
            };
            let script = script.to_string_lossy().into_owned();
            if i + 2 < args.len() {
                let rest: Vec<epsh::shell_bytes::ShellBytes> = args[i + 2..]
                    .iter()
                    .map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str()))
                    .collect();
                shell.set_args_bytes(&rest);
            }
            // exit propagates the interpreter's status (0 ok, 1-255 failure).
            std::process::exit(shell.run_script(&script));
        }
        if arg == "-s" {
            from_stdin = true;
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let mut unknown = false;
            for ch in arg[1..].chars() {
                match ch {
                    'e' => shell.opts_mut().errexit = true,
                    'u' => shell.opts_mut().nounset = true,
                    'x' => shell.opts_mut().xtrace = true,
                    'f' => shell.opts_mut().noglob = true,
                    _ => unknown = true,
                }
            }
            if unknown {
                eprintln!("ri-sh: unknown option {arg}");
                std::process::exit(2);
            }
            i += 1;
            continue;
        }
        // First non-option token: the script file (or, with `-s`, the start of
        // positional args).
        break;
    }

    if i >= args.len() {
        from_stdin = true;
    }

    if from_stdin {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            eprintln!("ri-sh: failed to read script from stdin");
            std::process::exit(2);
        }
        std::process::exit(shell.run_script(&src));
    }

    let file = args[i].to_string_lossy().into_owned();
    if i + 1 < args.len() {
        let rest: Vec<epsh::shell_bytes::ShellBytes> = args[i + 1..]
            .iter()
            .map(|s| epsh::shell_bytes::ShellBytes::from_os_str(s.as_os_str()))
            .collect();
        shell.set_args_bytes(&rest);
    }
    match std::fs::read_to_string(&file) {
        Ok(src) => std::process::exit(shell.run_script(&src)),
        Err(e) => {
            eprintln!("ri-sh: {file}: {e}");
            std::process::exit(2);
        }
    }
}
