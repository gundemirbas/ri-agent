//! `ri-sandbox` — the sandbox child binary.
//!
//! A single-purpose executable spawned by the main `ri` binary whenever a tool
//! is sandboxed. It performs the user-namespace + chroot pipeline itself and
//! starts **single-threaded, before any async runtime**, which is why
//! `unshare(CLONE_NEWUSER)` is legal here (and would fail in the parent, which
//! always runs under a multi-threaded tokio runtime).
//!
//! Usage: `ri-sandbox <image> [--binds=<host>/<guest>,…] -- <program> <args…>`
//!
//! Extra writable binds may also arrive via `$RI_SANDBOX_BINDS` (comma-separated
//! `host>guest` pairs). `/work` (the caller's cwd) and `/tmp` are bound
//! automatically.
//!
//! This file is intentionally minimal: no tokio, no clap, no logging — a fresh
//! process that either becomes the requested program or exits with a message.

#[path = "sys.rs"]
mod sys;

use std::path::Path;

#[derive(Debug, Clone)]
struct Opts {
    image: String,
    extra_binds: Vec<sys::Binds>,
    argv: Vec<String>,
}

/// Parse `image [--binds=…] -- prog args…`.
fn parse(args: &[String]) -> Result<Opts, String> {
    let mut image: Option<String> = None;
    let mut extra_binds: Vec<sys::Binds> = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut in_argv = false;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if in_argv {
            argv.push(a.clone());
        } else if a == "--" {
            in_argv = true;
        } else if let Some(list) = a.strip_prefix("--binds=") {
            for pair in list.split(',') {
                if pair.is_empty() {
                    continue;
                }
                let Some((host, guest)) = pair.split_once('>') else {
                    return Err(format!("bad bind {pair:?} (expected host>guest)"));
                };
                extra_binds.push(sys::bind(host, guest));
            }
        } else if image.is_none() && !a.starts_with('-') {
            image = Some(a.clone());
        } else {
            return Err(format!("unexpected argument {a:?}"));
        }
        i += 1;
    }

    // Binds may also come through the environment (set by the spawning side).
    if let Ok(env) = std::env::var("RI_SANDBOX_BINDS") {
        for pair in env.split(',').filter(|p| !p.is_empty()) {
            if let Some((host, guest)) = pair.split_once('>') {
                extra_binds.push(sys::bind(host, guest));
            }
        }
    }

    let Some(image) = image else {
        return Err("missing image path".to_string());
    };
    Ok(Opts {
        image,
        extra_binds,
        argv,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("sandbox: {e}");
            std::process::exit(2);
        }
    };
    if opts.argv.is_empty() {
        eprintln!("sandbox: no command given");
        std::process::exit(2);
    }
    match sys::run_child(Path::new(&opts.image), &opts.argv, &opts.extra_binds) {
        // run_child only returns on error (execvp replaces the process).
        Ok(()) => {}
        Err(e) => {
            eprintln!("sandbox: {e}");
            std::process::exit(1);
        }
    }
}
