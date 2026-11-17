//! hyprbeat binary entry (thin shell over the `hyprbeat` library facade —
//! the lib exists for `benches/`, see lib.rs).

use std::process::ExitCode;

fn main() -> ExitCode {
    // RUST_LOG=hyprbeat=debug for more detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("hyprbeat=info")),
        )
        .init();

    let args = match hyprbeat::cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("hyprbeat: {err}");
            eprintln!("run `hyprbeat --help` for usage");
            return ExitCode::FAILURE;
        }
    };

    if args.help {
        print!("{}", hyprbeat::cli::HELP);
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("hyprbeat {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let result = if let Some(osz_path) = &args.import {
        // --import (§5.2, phase 3): pure library operation — no render, no
        // audio device. Skips are reported, not failures (D-43/D-44); a
        // broken archive exits FAILURE without having written anything.
        // ImportError joins the crate's boxed Error here (its Display and
        // source() chain are what the error path prints).
        hyprbeat::beatmap::import::run_cli(osz_path, args.library_root.as_deref())
            .map(|_| ())
            .map_err(|e| Box::new(e) as hyprbeat::Error)
    } else if let Some(map_path) = &args.map {
        // --map (direct .osu path) — §10 row 3, visualizer commit a,
        // ruling 2026-11-17 (the strict per-flag split): load ONE file
        // through load_file — it must exist and be an .osu, no
        // set-key fallback — and exit: no render, no audio device yet
        // (the renderer + audio wiring land in commit b). LoadError
        // joins the crate's boxed Error like ImportError.
        hyprbeat::beatmap::load::run_map_cli(map_path)
            .map(|_| ())
            .map_err(|e| Box::new(e) as hyprbeat::Error)
    } else if let Some(set_key) = &args.set {
        // --set (library set key) — §10 row 3, visualizer commit a,
        // ruling 2026-11-17: resolve the key through the D-45 manifest
        // ONLY (load_set — no filesystem probe, so a file named KEY in
        // the working directory cannot hijack the lookup) and exit: no
        // render, no audio device yet (commit b wires them). LoadError
        // joins the crate's boxed Error like ImportError.
        hyprbeat::beatmap::load::run_set_cli(
            set_key,
            args.diff.as_deref(),
            args.library_root.as_deref(),
        )
        .map(|_| ())
        .map_err(|e| Box::new(e) as hyprbeat::Error)
    } else if args.headless {
        hyprbeat::render::headless::render_png(&args).map(|_| ())
    } else {
        hyprbeat::wayland::run(args)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hyprbeat: error: {err}");
            ExitCode::FAILURE
        }
    }
}
