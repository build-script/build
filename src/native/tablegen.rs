// TableGen of LLVM and MLIR: headers like StandaloneOps.h.inc from .td files, into build/generated. A header is
// written again when its .td file, one that file includes or its command changed: TableGen names them in a depfile.
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::system;

use super::compile::RecordedCommand;

/// `entries` are lists of the tool, the .td file, the header and the arguments: "mlir-tblgen",
/// "include/Ops.td", "Ops.h.inc", "-gen-op-decls". The tool is one of `tools_dirs` or of the PATH.
pub fn generate(
    module: &str,
    entries: &[Vec<String>],
    module_dir: &Path,
    include_flags: &[String],
    tools_dirs: &[PathBuf],
    generated_dir: &Path,
) -> Result<(), String> {
    let mut outdated = vec![];

    for entry in entries {
        let [tool, td_file, header, arguments @ ..] = entry.as_slice() else {
            return Err(format!("tablegen takes the tool, the .td file and the header, then arguments: {entry:?}"));
        };

        let program = tools_dirs.iter()
            .map(|dir| dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX)))
            .find(|program| program.is_file())
            .or_else(|| system::find_program(tool))
            .ok_or_else(|| format!("{tool} is in no tools directory of the dependencies nor in the PATH"))?;

        let output = generated_dir.join(header);
        let mut line = vec![program.display().to_string(), module_dir.join(td_file).display().to_string()];
        line.extend(include_flags.iter().cloned());
        line.extend(arguments.iter().cloned());
        line.extend(["-o".to_string(), output.display().to_string()]);

        let recorded = RecordedCommand::new(output, line);
        if !recorded.is_up_to_date() {
            outdated.push(recorded);
        }
    }

    if outdated.is_empty() {
        return Ok(());
    }

    system::log(module, &format!("Generating {} file(s) with TableGen", outdated.len()));

    for recorded in outdated {
        system::create_parent_dir(&recorded.output)?;

        let mut command = Command::new(&recorded.line[0]);
        command.args(&recorded.line[1..])
            .arg("-d")
            .arg(&recorded.depfile)
            .current_dir(module_dir);
        system::run_command_quietly(&mut command, &format!("generating {}", recorded.output.display()))?;

        recorded.record()?;
    }

    return Ok(());
}
