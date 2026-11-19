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
        // --map (direct .osu path) — §10 row 3, visualizer commit b:
        // load ONE file through the commit-a loader (ruling 2026-11-17,
        // the strict per-flag split: it must exist and be an .osu, no
        // set-key fallback; the D-38 summary line still logs), then
        // render — live on the desktop, or one still frame with
        // --headless. LoadError joins the crate's boxed Error like
        // ImportError.
        hyprbeat::beatmap::load::run_map_cli(map_path)
            .map_err(|e| Box::new(e) as hyprbeat::Error)
            .and_then(|map| dispatch_map(&args, map))
    } else if let Some(set_key) = &args.set {
        // --set (library set key) — §10 row 3, visualizer commit b:
        // resolve the key through the D-45 manifest ONLY (load_set — no
        // filesystem probe, so a file named KEY in the working
        // directory cannot hijack the lookup), then the same
        // load-then-dispatch as --map.
        hyprbeat::beatmap::load::run_set_cli(
            set_key,
            args.diff.as_deref(),
            args.library_root.as_deref(),
        )
        .map_err(|e| Box::new(e) as hyprbeat::Error)
        .and_then(|map| dispatch_map(&args, map))
    } else if args.headless {
        hyprbeat::render::headless::render_png(&args, None).map(|_| ())
    } else {
        hyprbeat::wayland::run(args, None)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hyprbeat: error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Visualizer commit b dispatch: the map is loaded, now render it —
/// live on the desktop, or as one still frame with `--headless`.
/// `--testcard` is not implied by `--headless` anymore (Δ-F): the
/// still frame is the map's timeline view unless the flag is passed
/// explicitly.
fn dispatch_map(
    args: &hyprbeat::cli::Args,
    map: hyprbeat::beatmap::load::LoadedMap,
) -> Result<(), hyprbeat::Error> {
    if args.headless {
        hyprbeat::render::headless::render_png(args, Some(&map)).map(|_| ())
    } else {
        hyprbeat::wayland::run(args.clone(), Some(map))
    }
}
