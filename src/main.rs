#![forbid(unsafe_code)]

use tokio::runtime::{Builder, Runtime};

fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let result = match arguments.first().map(String::as_str) {
        Some(command @ ("auth" | "serve" | "doctor" | "health" | "portal")) => {
            match runtime(command == "serve") {
                Ok(runtime) => runtime.block_on(kanata::cli::run_async(arguments)),
                Err(_) => Err("could not initialize command runtime".into()),
            }
        }
        _ => kanata::cli::run(arguments),
    };

    match result {
        Ok(None) => println!("{} {}", kanata::NAME, kanata::VERSION),
        // Empty when the command already wrote its own output.
        Ok(Some(message)) if message.is_empty() => {}
        Ok(Some(message)) => println!("{message}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

/// Builds a multi-thread runtime for `serve` and a current-thread one otherwise.
fn runtime(serve: bool) -> std::io::Result<Runtime> {
    if !serve {
        return Builder::new_current_thread().enable_all().build();
    }
    let workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(2, 8);
    Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("kanata-worker")
        .enable_all()
        .build()
}
