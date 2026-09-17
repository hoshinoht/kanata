use std::process::{Command, Output};

pub fn run_kanata() -> Output {
    Command::new(env!("CARGO_BIN_EXE_kanata"))
        .output()
        .expect("kanata binary should run")
}
