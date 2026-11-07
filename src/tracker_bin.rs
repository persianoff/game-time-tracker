#![windows_subsystem = "windows"]

use gametime::{config, log};

fn main() {
    let Ok(dir) = config::data_dir() else { return };
    log::init(&dir, false);
    if let Err(e) = config::Config::load_or_create(&dir) {
        log::log_line(&format!("config error: {e}"));
        return;
    }
    log::log_line("tracker started");
}
