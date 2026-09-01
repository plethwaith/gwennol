//! `cargo xtask <command>`.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask bundle [--out <dir>]

  bundle   Compile every guest-backed plugin under plugins/ to wasm and
           write the manifests with their wasmModules slots filled
           (default: target/bundle/).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bundle") => bundle(&args[1..]),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn bundle(args: &[String]) -> ExitCode {
    let workspace = xtask::workspace_root();
    let mut out_dir = workspace.join(xtask::BUNDLE_DIR);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => match args.next() {
                Some(dir) => out_dir = PathBuf::from(dir),
                None => {
                    eprintln!("--out needs a directory\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let bundled = match xtask::bundle(&workspace) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bundle failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let written = match xtask::write_bundle(&bundled, &out_dir) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("bundle failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (plugin, path) in bundled.iter().zip(written) {
        let guests = if plugin.guests.is_empty() {
            "declarative".to_string()
        } else {
            plugin
                .guests
                .iter()
                .map(|g| g.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("{} ({guests}) -> {}", plugin.name(), path.display());
    }
    ExitCode::SUCCESS
}
