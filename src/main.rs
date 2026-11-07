mod auth;
mod engine;
mod skills;
mod ui;

use engine::{INTERRUPTED, Workspace, check_interrupt};
use llm_api::Result;
use std::sync::atomic::Ordering;

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}
fn install_interrupt_handler() {
    // SAFETY: handler only sets a lock-free atomic flag and never calls Rust I/O.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t);
    }
}
fn main() {
    install_interrupt_handler();
    if let Err(error) = ui::run() {
        eprintln!("agent: {}", ui::display(&error.to_string()));
        std::process::exit(
            if INTERRUPTED.load(Ordering::Relaxed) || error.is::<llm_api::Cancelled>() {
                130
            } else {
                1
            },
        );
    }
}

#[cfg(test)]
use {
    engine::{OUTPUT_LIMIT, atomic_write, limited, shell, text, tools, turn},
    llm_api::{Client, Message, Provider, Request, Response},
    serde_json::{Value, json},
    std::{
        fs::{self, File},
        io::{self, IsTerminal, Read, Write},
        os::fd::AsRawFd,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    },
    ui::{chat, display},
};
#[cfg(test)]
#[path = "../tests/unit/mod.rs"]
mod tests;
