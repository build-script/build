// The compilers of C and C++ modules: those of the system
use std::process::Command;

/// Compiler and archiver
pub struct Toolchain {
    pub c_compiler: Vec<String>,
    pub cpp_compiler: Vec<String>,
    pub archiver: Vec<String>,
}

impl Toolchain {
    /// cc, c++ and ar of the system, or those CC, CXX and AR name, like make takes them
    pub fn system() -> Self {
        let from_environment = |variable: &str, default: &str| vec![std::env::var(variable).unwrap_or(default.to_string())];

        return Toolchain {
            c_compiler: from_environment("CC", "cc"),
            cpp_compiler: from_environment("CXX", "c++"),
            archiver: from_environment("AR", "ar"),
        };
    }

    pub fn command(&self, program: &[String]) -> Command {
        let mut command = Command::new(&program[0]);
        command.args(&program[1..]);

        return command;
    }
}
