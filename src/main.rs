fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let result = if arguments
        .first()
        .is_some_and(|argument| argument == "auth" || argument == "serve")
    {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(kanata::cli::run_async(arguments)),
            Err(_) => Err("could not initialize command runtime".into()),
        }
    } else {
        kanata::cli::run(arguments)
    };

    match result {
        Ok(None) => println!("{} {}", kanata::NAME, kanata::VERSION),
        Ok(Some(message)) => println!("{message}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}
