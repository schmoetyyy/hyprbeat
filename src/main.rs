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

    let result = if args.headless {
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
