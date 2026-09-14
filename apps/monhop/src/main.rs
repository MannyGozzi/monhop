mod cli;
mod diagnostics;
mod identity_storage;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = cli::parse(&args)
        .map_err(str::to_owned)
        .and_then(diagnostics::run);
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("MonHop: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
