fn main() {
    if let Err(err) = codex_copilot::cli::main() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
