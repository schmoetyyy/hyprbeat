//! hyprbeat — an open-source rhythm game that lives on your desktop
//! background, compatible with the osu! beatmap format. Built for
//! Hyprland/Wayland.
//!
//! Phase 1 (CONTEXT.md §10): Cargo scaffold, layer-shell background surface,
//! frame-callback render loop, `--headless` PNG output, fractional-scale
//! correctness, and the Q-1/Q-2 input probe. Audio, beatmaps, gameplay and
//! the config file arrive in phases 2–8.

mod cli;
mod config;
mod render;
mod wayland;

use std::process::ExitCode;

/// Crate-wide error type. Phase 1 keeps it dependency-free (§4); a dedicated
/// error enum can replace it when the config parser lands in phase 5.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

fn main() -> ExitCode {
    // RUST_LOG=hyprbeat=debug for more detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("hyprbeat=info")),
        )
        .init();

    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("hyprbeat: {err}");
            eprintln!("run `hyprbeat --help` for usage");
            return ExitCode::FAILURE;
        }
    };

    if args.help {
        print!("{}", cli::HELP);
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("hyprbeat {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let result = if args.headless {
        render::headless::render_png(&args).map(|_| ())
    } else {
        wayland::run(args)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hyprbeat: error: {err}");
            ExitCode::FAILURE
        }
    }
}
