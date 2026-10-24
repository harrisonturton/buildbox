//! The container helper on its own, for tests. In buildbox it is part of
//! the main binary.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    container_namespaces::helper_main(&args)
}
