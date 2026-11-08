use std::process::ExitCode;

const USAGE: &str = "Usage: gametime <command>

Commands:
  scan         Scan for installed games
  status       Show tracker status
  export       Export data
  autostart    Manage autostart
  stop         Stop the tracker
  run [--verbose]
               Run the tracker in the foreground
  --help       Show this help";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stop") => {
            if gametime::win32::signal_stop(gametime::tracker::STOP_EVENT_NAME) {
                println!("stopped");
                ExitCode::SUCCESS
            } else {
                println!("tracker not running");
                ExitCode::FAILURE
            }
        }
        Some("autostart") => ExitCode::from(gametime::autostart::cli(&args)),
        Some("scan") => gametime::cli_info::scan(&args[1..]),
        Some("status") => gametime::cli_info::status(&args[1..]),
        Some("export") => gametime::export::cli(&args[1..]),
        Some("run") => {
            if args[1..].iter().any(|a| a != "--verbose") {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
            let verbose = args.len() > 1;
            ExitCode::from(gametime::tracker::run(verbose, false) as u8)
        }
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
