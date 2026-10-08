//! Log to `~/.local/state/lingo/lingo.log` (and to the terminal, if there is one).

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::config;

const MAX_BYTES: u64 = 1_000_000;

struct Logger {
    file: Option<Mutex<File>>,
    stderr: bool,
}

impl Log for Logger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= Level::Warn || (meta.level() <= Level::Info && meta.target().starts_with("lingo"))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let line = format!("{now} {} {}: {}\n", record.level(), record.target(), record.args());
        if let Some(file) = &self.file {
            let _ = file.lock().unwrap().write_all(line.as_bytes());
        }
        if self.stderr {
            eprint!("{line}");
        }
    }

    fn flush(&self) {}
}

/// Opens the log, keeping the two previous versions once it grows past 1 MB.
pub fn init() {
    let dir = config::state_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("lingo.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(dir.join("lingo.log.1"), dir.join("lingo.log.2"));
        let _ = fs::rename(&path, dir.join("lingo.log.1"));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok().map(Mutex::new);
    let logger = Logger { file, stderr: std::io::stderr().is_terminal() };
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
