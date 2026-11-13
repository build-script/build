// Compiling C and C++: every source on its own, in parallel, and only when its object is older than
// the source, one of its headers, its command line or the version of its compiler. clang and gcc name
// the headers in a depfile.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::common::semaphore::Semaphore;
use crate::common::up_to_date::hash_hex;
use crate::common::{system, terminal};

use super::toolchain::{NativeTarget, Toolchain};

static CORES: LazyLock<usize> = LazyLock::new(|| std::thread::available_parallelism().map_or(4, |count| count.get()));

/// Compilers running now, of all modules at most one per core: modules build at the same time
static COMPILERS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(*CORES));

/// What a module is written in: build_c takes .c files, build_cpp also .cc, .cpp and .cxx
#[derive(Debug, Clone, Copy, PartialEq, Hash)]
pub enum Language {
    C,
    Cpp,
}

impl Language {
    /// Of its sources, the rest are headers
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Language::C => return &["c"],
            Language::Cpp => return &["c", "cc", "cpp", "cxx"],
        }
    }
}

/// The sources of a module or of its tests and how they are compiled
pub struct Compilation<'a> {
    pub module_name: &'a str,
    pub language: Language,
    pub toolchain: &'a Toolchain,
    /// relative to it the objects keep the paths of their sources
    pub module_dir: &'a Path,
    /// globs relative to the module, like "src/**" or "examples/simple.cpp"
    pub source_patterns: &'a [String],
    pub exclude_patterns: &'a [String],
    /// -I and -D, the standards come with c_standard and cpp_standard
    pub flags: Vec<String>,
    pub c_standard: &'a str,
    pub cpp_standard: &'a str,
    pub objects_dir: PathBuf,
    /// for the log
    pub tests: bool,
    /// the files below the roots of the patterns, searched once
    pub candidates: OnceLock<Vec<PathBuf>>,
    /// what commands the module depends on wrote, compiled with its sources
    pub generated_sources: Vec<PathBuf>,
}

impl Compilation<'_> {
    /// The files `source_patterns` match, without those `exclude_patterns` match, and the generated ones
    pub fn sources(&self) -> Vec<PathBuf> {
        return self.candidates()
            .iter()
            .filter(|file| self.matches_any(self.source_patterns, file) && !self.matches_any(self.exclude_patterns, file))
            .chain(&self.generated_sources)
            .cloned()
            .collect();
    }

    /// Whether one of the patterns matches the file, relative to the module directory
    pub fn matches_any(&self, patterns: &[String], file: &Path) -> bool {
        return patterns.iter().any(|pattern| system::matches_glob(pattern, &system::relative_with_slashes(self.module_dir, file)));
    }

    /// A pattern of `source_patterns` that matches no source, most likely misspelled
    pub fn unmatched_pattern(&self) -> Option<&String> {
        let sources: Vec<String> = self.candidates()
            .iter()
            .map(|file| system::relative_with_slashes(self.module_dir, file))
            .collect();
        return self.source_patterns.iter().find(|pattern| !sources.iter().any(|source| system::matches_glob(pattern, source)));
    }

    fn candidates(&self) -> &[PathBuf] {
        return self.candidates.get_or_init(|| self.search_candidates());
    }

    /// The sources of the language below the roots of the patterns
    fn search_candidates(&self) -> Vec<PathBuf> {
        return system::files_below_roots(self.module_dir, self.source_patterns, |file| {
            let extension = file.extension()
                .unwrap_or_default()
                .to_string_lossy();

            return self.language.extensions().contains(&extension.as_ref());
        });
    }

    /// src/main.cpp becomes <objects>/src/main.cpp.o
    pub fn object(&self, source: &Path) -> PathBuf {
        // generated outside of the module: by the directory it is in, two commands may write the same name
        if !source.starts_with(self.module_dir) {
            let directory = hash_hex(source.parent());
            let name = source.file_name()
                .unwrap_or_default()
                .to_string_lossy();
            return self.objects_dir.join("generated").join(&directory[..8]).join(format!("{name}.o"));
        }

        return self.objects_dir.join(format!("{}.o", system::relative_with_slashes(self.module_dir, source)));
    }

    /// The compiler with its arguments, without the depfile
    fn command_line(&self, source: &Path) -> Vec<String> {
        let standard = match source.extension().is_some_and(|extension| extension == "c") {
            true => format!("-std={}", self.c_standard),
            false => format!("-std={}", self.cpp_standard),
        };

        let mut line = self.compiler(source).to_vec();
        line.push(standard);
        line.extend(self.flags.iter().cloned());
        line.extend(["-c".to_string(), source.display().to_string()]);
        line.extend(["-o".to_string(), self.object(source).display().to_string()]);

        return line;
    }

    /// The command line after the version of its compiler, as the .o.line next to the object keeps it:
    /// after an update of Xcode or GCC every source is compiled again
    fn recorded_line(&self, source: &Path) -> Vec<String> {
        return [vec![self.toolchain.version(self.compiler(source))], self.command_line(source)].concat();
    }

    /// The C compiler for .c files, the C++ compiler for the others
    fn compiler(&self, source: &Path) -> &[String] {
        match source.extension().is_some_and(|extension| extension == "c") {
            true => return &self.toolchain.c_compiler,
            false => return &self.toolchain.cpp_compiler,
        }
    }

    /// The objects of all sources, and whether one had to be compiled
    pub fn run(&self) -> Result<(Vec<PathBuf>, bool), String> {
        let sources = self.sources();
        let outdated: Vec<&PathBuf> =
            sources.iter()
                .filter(|source| !self.is_up_to_date(source))
                .collect();
        let objects = sources.iter()
            .map(|source| self.object(source))
            .collect();

        if outdated.is_empty() {
            return Ok((objects, false));
        }

        let files = if self.tests { "test file(s)" } else { "file(s)" };
        let language = if self.language == Language::C { "C" } else { "C/C++" };
        system::log(self.module_name, &format!("Compiling {} {language} {files}", outdated.len()));

        let threads = CORES.min(outdated.len());
        let queue = Mutex::new(outdated);
        let errors = Mutex::new(vec![]);

        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    while let Some(source) = queue.lock().unwrap().pop() {
                        if let Err(error) = self.compile(source) {
                            errors.lock()
                                .unwrap()
                                .push(error);
                        }
                    }
                });
            }
        });

        if let Some(error) = errors.into_inner().unwrap().into_iter().next() {
            return Err(error);
        }

        return Ok((objects, true));
    }

    /// With a depfile and the recorded line next to the object, for the next build
    fn compile(&self, source: &Path) -> Result<(), String> {
        let object = self.object(source);
        let line = self.command_line(source);
        system::create_parent_dir(&object)?;

        let compiler = self.compiler(source);
        let recorded = RecordedCommand::new(object.clone(), self.recorded_line(source));
        let mut arguments = line[compiler.len()..].to_vec();
        arguments.extend(["-MMD".to_string(), "-MF".to_string(), recorded.depfile.display().to_string()]);

        // the output goes through a pipe, the compiler would leave out its colors
        if terminal::colors() {
            arguments.push("-fdiagnostics-color=always".to_string());
        }

        // relative paths in the flags count from the module, like in compile_commands.json
        let mut command = self.toolchain.command_with_arguments(compiler, &arguments, &object.with_extension("o.rsp"))?;
        command.current_dir(self.module_dir);

        let _compiler = COMPILERS.acquire();
        system::run_command_in_one_piece(&mut command, &format!("compiling {}", source.display()))?;

        return recorded.record();
    }

    fn is_up_to_date(&self, source: &Path) -> bool {
        return RecordedCommand::new(self.object(source), self.recorded_line(source)).is_up_to_date();
    }

    /// For clangd, which finds build/compile_commands.json of the module from its sources
    pub fn compile_commands(&self) -> Vec<serde_json::Value> {
        return self.sources()
            .iter()
            .map(|source| {
                serde_json::json!({
                    "directory": self.module_dir,
                    "file": source,
                    "arguments": self.command_line(source),
                    "output": self.object(source),
                })
            })
            .collect();
    }
}

/// The output of one command, like the object of a source: up to date while it is newer than every input
/// its depfile names and its .line file holds the same command line
pub struct RecordedCommand {
    pub output: PathBuf,
    /// <output>.d, the tool writes it
    pub depfile: PathBuf,
    /// what <output>.line keeps: a new compiler or another flag builds it again
    pub line: Vec<String>,
}

impl RecordedCommand {
    pub fn new(output: PathBuf, line: Vec<String>) -> Self {
        let depfile = PathBuf::from(format!("{}.d", output.display()));
        return RecordedCommand { output, depfile, line };
    }

    pub fn is_up_to_date(&self) -> bool {
        let modified = |path: &Path| fs::metadata(path).and_then(|metadata| metadata.modified());

        let (Ok(built), Ok(depfile), Ok(stored_line)) =
            (modified(&self.output), fs::read_to_string(&self.depfile), fs::read_to_string(self.line_file()))
        else {
            return false;
        };

        if stored_line != self.line.join("\n") {
            return false;
        }

        return depfile_inputs(&depfile)
            .iter()
            .all(|input| modified(Path::new(input)).is_ok_and(|changed| changed <= built));
    }

    /// Once the command succeeded, for the next build
    pub fn record(&self) -> Result<(), String> {
        return system::write_file(&self.line_file(), &self.line.join("\n"));
    }

    fn line_file(&self) -> PathBuf {
        return PathBuf::from(format!("{}.line", self.output.display()));
    }
}

/// The inputs of a depfile, `output: input input ...`: \ escapes spaces in names and ends continued
/// lines, $$ is a $. Paths of Windows keep their \, the : of their drive is followed by none.
fn depfile_inputs(depfile: &str) -> Vec<String> {
    let inputs = depfile.split_once(": ")
        .map(|(_, inputs)| inputs)
        .unwrap_or("");

    let mut names = vec![];
    let mut name = String::new();
    let mut characters = inputs.chars().peekable();

    while let Some(character) = characters.next() {
        match (character, characters.peek()) {
            ('\\', Some(' ')) | ('$', Some('$')) => name.push(characters.next().unwrap_or(character)),
            ('\\', Some('\r' | '\n')) => {}
            (' ' | '\t' | '\r' | '\n', _) => names.extend((!name.is_empty()).then(|| std::mem::take(&mut name))),
            _ => name.push(character),
        }
    }

    names.extend((!name.is_empty()).then_some(name));
    return names;
}

/// A static library of the objects
pub fn archive(toolchain: &Toolchain, objects: &[PathBuf], library: &Path) -> Result<(), String> {
    system::create_parent_dir(library)?;
    let _ = fs::remove_file(library);

    let mut arguments = vec!["rcs".to_string(), library.display().to_string()];
    arguments.extend(objects.iter().map(|object| object.display().to_string()));

    let response_file = PathBuf::from(format!("{}.rsp", library.display()));
    let mut command = toolchain.command_with_arguments(&toolchain.archiver, &arguments, &response_file)?;
    return system::run_command_in_one_piece(&mut command, "archiving");
}

/// A program or shared library of the objects and libraries, with the C++ compiler when C++ is among
/// them: it brings the C++ standard library. A library several modules need is linked once, after
/// everything that needs it.
pub fn link(
    toolchain: &Toolchain,
    cpp: bool,
    objects: &[PathBuf],
    link_flags: &[String],
    program: &Path,
) -> Result<(), String> {
    system::create_parent_dir(program)?;

    let mut arguments: Vec<String> = objects.iter()
        .map(|object| object.display().to_string())
        .collect();
    arguments.extend(linked_once(link_flags));
    arguments.extend(["-o".to_string(), program.display().to_string()]);

    if !cpp && toolchain.target.links_math_library() {
        arguments.push("-lm".to_string());
    }

    // like CMake and Visual Studio every program links the libraries of Windows. link.exe takes a program
    // for the console only by a main() among the objects, that of gtest_main is in a library.
    arguments.extend(toolchain.target.system_libraries().iter().map(|library| format!("-l{library}")));

    let windows_program = toolchain.target == NativeTarget::Windows && program.extension().is_some_and(|extension| extension == "exe");
    if windows_program && !toolchain.zig {
        arguments.push("-Wl,-subsystem:console".to_string());
    }

    let linker = if cpp { &toolchain.cpp_compiler } else { &toolchain.c_compiler };
    let response_file = PathBuf::from(format!("{}.rsp", program.display()));
    let mut command = toolchain.command_with_arguments(linker, &arguments, &response_file)?;
    return system::run_command_in_one_piece(&mut command, "linking");
}

/// The flags, a library several modules need once, after the last that needs it: a linker takes static
/// libraries in order
fn linked_once(link_flags: &[String]) -> Vec<String> {
    let is_library = |flag: &String| flag.starts_with("-l") || flag.ends_with(".a") || flag.ends_with(".lib");
    let mut seen = HashSet::new();
    let mut flags: Vec<String> = link_flags.iter()
        .rev()
        .filter(|flag| !is_library(flag) || seen.insert(*flag))
        .cloned()
        .collect();
    flags.reverse();

    return flags;
}

#[cfg(test)]
mod tests {
    use super::{depfile_inputs, linked_once};

    #[test]
    fn libraries_are_linked_once_after_the_last_that_needs_them() {
        let flags = ["libapp.a", "-lfmt", "-Wl,-rpath,x", "libcore.a", "-lfmt", "-pthread", "-pthread"].map(String::from);
        assert_eq!(linked_once(&flags), ["libapp.a", "-Wl,-rpath,x", "libcore.a", "-lfmt", "-pthread", "-pthread"]);
    }

    #[test]
    fn depfiles_name_inputs_with_spaces_and_drives() {
        let depfile = "obj/main.cpp.o: src/main.cpp include/my\\ header.h \\\n  include/cost$$.h\n";
        assert_eq!(depfile_inputs(depfile), ["src/main.cpp", "include/my header.h", "include/cost$.h"]);

        let depfile = "D:\\a\\obj\\main.cpp.o: D:\\a\\src\\main.cpp \\\r\n  C:\\Program\\ Files\\x.h\r\n";
        assert_eq!(depfile_inputs(depfile), ["D:\\a\\src\\main.cpp", "C:\\Program Files\\x.h"]);
    }
}
