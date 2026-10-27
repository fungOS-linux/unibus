use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("usage: unibus-targets openapi <service> <document> [--json]");
        return ExitCode::from(2);
    };
    match command.as_str() {
        "openapi" => {
            let Some(service) = args.next() else {
                eprintln!("usage: unibus-targets openapi <service> <document> [--json]");
                return ExitCode::from(2);
            };
            let Some(path) = args.next() else {
                eprintln!("usage: unibus-targets openapi <service> <document> [--json]");
                return ExitCode::from(2);
            };
            let path = PathBuf::from(path);
            let document = match std::fs::read_to_string(&path) {
                Ok(document) => document,
                Err(error) => {
                    eprintln!("read {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            match unibus_targets::from_openapi(&service, &path.display().to_string(), &document) {
                Ok(index) => match index.to_json() {
                    Ok(json) => {
                        println!("{json}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        ExitCode::FAILURE
                    }
                },
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            ExitCode::from(2)
        }
    }
}
