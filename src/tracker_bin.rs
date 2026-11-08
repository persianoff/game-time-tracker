#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(gametime::tracker::run(false));
}
