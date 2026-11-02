// Compiling C and C++: every source on its own, in parallel, and only when its object is older than
// the source, one of its headers, its command line or the version of its compiler. clang and gcc name
// the headers in a depfile.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex};

use crate::common::{system, terminal};

use super::toolchain::Toolchain;

/// Compilers running now, of all modules at most one per core: modules build at the same time
static RUNNING: Mutex<usize> = Mutex::new(0);
static FINISHED: Condvar = Condvar::new();
static CORES: LazyLock<usize> = LazyLock::new(|| std::thread::available_parallelism().map_or(4, |count| count.get()));

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
}

impl Compilation<'_> {
    /// The files `source_patterns` match, without those `exclude_patterns` match
    pub fn sources(&self) -> Vec<PathBuf> {
        return self.candidates()
            .into_iter()
            .filter(|file| self.matches_any(self.source_patterns, file) && !self.matches_any(self.exclude_patterns, file))
            .collect();
    }

    /// Whether one of the patterns matches the file, relative to the module directory
    pub fn matches_any(&self, patterns: &[String], file: &Path) -> bool {
        return patterns.iter().any(|pattern| matches_glob(pattern, &self.relative(file)));
    }

    /// A pattern of `source_patterns` that matches no source, most likely misspelled
    pub fn unmatched_pattern(&self) -> Option<&String> {
        let sources: Vec<String> = self.candidates()
            .iter()
            .map(|file| self.relative(file))
            .collect();
        return self.source_patterns.iter().find(|pattern| !sources.iter().any(|source| matches_glob(pattern, source)));
    }

    /// The sources below the roots of the patterns, but in build/ and hidden directories like .git
    fn candidates(&self) -> Vec<PathBuf> {
        let mut sources = vec![];
        let mut pending = source_roots(self.module_dir, self.source_patterns);

        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
                let path = entry.path();
                let hidden = entry.file_name()
                    .to_string_lossy()
                    .starts_with('.');
                let extension = path.extension()
                    .unwrap_or_default()
                    .to_string_lossy();

                match path.is_dir() {
                    true if !hidden && path != system::build_dir(self.module_dir) => pending.push(path),
                    false if self.language.extensions().contains(&extension.as_ref()) => sources.push(path),
                    _ => {}
                }
            }
        }

        // roots may be below each other
        sources.sort();
        sources.dedup();
        return sources;
    }

    /// Relative to the module directory, like the patterns, with / on Windows too
    fn relative(&self, file: &Path) -> String {
        let relative = file.strip_prefix(self.module_dir)
            .unwrap_or(file)
            .display()
            .to_string();

        return match cfg!(windows) {
            true => relative.replace('\\', "/"),
            false => relative,
        };
    }

    /// src/main.cpp becomes <objects>/src/main.cpp.o
    pub fn object(&self, source: &Path) -> PathBuf {
        return self.objects_dir.join(format!("{}.o", self.relative(source)));
    }

    /// The compiler with its arguments, without the depfile
    fn command_line(&self, source: &Path) -> Vec<String> {
        let c_source = source.extension().is_some_and(|extension| extension == "c");

        let (compiler, standard) = match c_source {
            true => (&self.toolchain.c_compiler, format!("-std={}", self.c_standard)),
            false => (&self.toolchain.cpp_compiler, format!("-std={}", self.cpp_standard)),
        };

        let mut line = compiler.clone();
        line.push(standard);
        line.extend(self.flags.iter().cloned());
        line.extend(["-c".to_string(), source.display().to_string()]);
        line.extend(["-o".to_string(), self.object(source).display().to_string()]);

        return line;
    }

    /// The command line after the version of its compiler, as the .o.line next to the object keeps it:
    /// after an update of Xcode or GCC every source is compiled again
    fn recorded_line(&self, source: &Path) -> Vec<String> {
        let c_source = source.extension().is_some_and(|extension| extension == "c");
        let compiler = if c_source { &self.toolchain.c_compiler } else { &self.toolchain.cpp_compiler };

        return [vec![self.toolchain.version(compiler)], self.command_line(source)].concat();
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

        // relative paths in the flags count from the module, like in compile_commands.json
        let mut command = self.toolchain.command(&line);
        command.current_dir(self.module_dir);
        command.arg("-MMD")
            .arg("-MF")
            .arg(object.with_extension("o.d"));

        // the output goes through a pipe, the compiler would leave out its colors
        if terminal::colors() {
            command.arg("-fdiagnostics-color=always");
        }

        let _compiler = Compiler::start();
        system::run_command_in_one_piece(&mut command, &format!("compiling {}", source.display()))?;

        return system::write_file(&object.with_extension("o.line"), &self.recorded_line(source).join("\n"));
    }

    fn is_up_to_date(&self, source: &Path) -> bool {
        let object = self.object(source);
        let (depfile, line) = (object.with_extension("o.d"), object.with_extension("o.line"));

        return output_is_up_to_date(&object, &depfile, &line, &self.recorded_line(source));
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

/// One of the compilers running now, the next may start when it is dropped
struct Compiler;

impl Compiler {
    /// Waits until fewer compilers than cores run
    fn start() -> Self {
        let mut running = RUNNING.lock().unwrap();

        while *running >= *CORES {
            running = FINISHED.wait(running).unwrap();
        }

        *running += 1;
        return Compiler;
    }
}

impl Drop for Compiler {
    fn drop(&mut self) {
        *RUNNING.lock().unwrap() -= 1;
        FINISHED.notify_one();
    }
}

/// The directories of the patterns up to their first * or ?, of a pattern without them that of its file:
/// src of "src/**", the module of "*.c". Their sources are below them, private headers next to those.
pub fn source_roots(module_dir: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![];

    for pattern in patterns {
        let segments: Vec<&str> = pattern.split('/').collect();
        let fixed = segments.iter()
            .take_while(|segment| !segment.contains(['*', '?']))
            .count();
        let fixed = &segments[..fixed.min(segments.len() - 1)];
        let root = if fixed.is_empty() { module_dir.to_path_buf() } else { module_dir.join(fixed.join("/")) };

        if !roots.contains(&root) {
            roots.push(root);
        }
    }

    return roots;
}

/// The output is newer than every input its depfile names, and `line_file` holds this command line
pub fn output_is_up_to_date(output: &Path, depfile: &Path, line_file: &Path, line: &[String]) -> bool {
    let modified = |path: &Path| fs::metadata(path).and_then(|metadata| metadata.modified());

    let (Ok(built), Ok(depfile), Ok(stored_line)) =
        (modified(output), fs::read_to_string(depfile), fs::read_to_string(line_file))
    else {
        return false;
    };

    if stored_line != line.join("\n") {
        return false;
    }

    return depfile_inputs(&depfile)
        .iter()
        .all(|input| modified(Path::new(input)).is_ok_and(|changed| changed <= built));
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

/// `*` and `?` within a name, `**` for any directories: "src/**/*.cpp", "testes/**"
fn matches_glob(pattern: &str, path: &str) -> bool {
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.first(), path.first()) {
            (None, None) => return true,
            (Some(&"**"), _) => {
                return segments(&pattern[1..], path) || (!path.is_empty() && segments(pattern, &path[1..]));
            }
            (Some(name_pattern), Some(name)) => {
                return name_matches(name_pattern.as_bytes(), name.as_bytes()) && segments(&pattern[1..], &path[1..]);
            }
            _ => return false,
        }
    }

    fn name_matches(pattern: &[u8], name: &[u8]) -> bool {
        match (pattern.first(), name.first()) {
            (None, None) => return true,
            (Some(b'*'), _) => return name_matches(&pattern[1..], name) || (!name.is_empty() && name_matches(pattern, &name[1..])),
            (Some(b'?'), Some(_)) => return name_matches(&pattern[1..], &name[1..]),
            (Some(expected), Some(found)) => return expected == found && name_matches(&pattern[1..], &name[1..]),
            _ => return false,
        }
    }

    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();

    return segments(&pattern, &path);
}

/// A static library of the objects
pub fn archive(toolchain: &Toolchain, objects: &[PathBuf], library: &Path) -> Result<(), String> {
    system::create_parent_dir(library)?;
    let _ = fs::remove_file(library);

    let mut command = toolchain.command(&toolchain.archiver);
    command.arg("rcs")
        .arg(library)
        .args(objects);

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

    let is_library = |flag: &String| flag.starts_with("-l") || flag.ends_with(".a") || flag.ends_with(".lib");
    let mut seen = HashSet::new();
    let mut flags: Vec<&String> = link_flags.iter()
        .rev()
        .filter(|flag| !is_library(flag) || seen.insert(*flag))
        .collect();
    flags.reverse();

    let linker = if cpp { &toolchain.cpp_compiler } else { &toolchain.c_compiler };
    let mut command = toolchain.command(linker);
    command.args(objects)
        .args(flags)
        .arg("-o")
        .arg(program);

    // the C++ standard library brings the math library along, on macOS libSystem has it
    if !cpp && cfg!(target_os = "linux") {
        command.arg("-lm");
    }

    return system::run_command_in_one_piece(&mut command, "linking");
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{depfile_inputs, matches_glob, source_roots};

    #[test]
    fn patterns_are_searched_below_their_roots() {
        let patterns = ["src/**", "*.c", "examples/simple.cpp", "src/**/*.cc"].map(String::from);
        let roots = source_roots(Path::new("/module"), &patterns);

        assert_eq!(roots, ["/module/src", "/module", "/module/examples"].map(PathBuf::from));
    }

    #[test]
    fn depfiles_name_inputs_with_spaces_and_drives() {
        let depfile = "obj/main.cpp.o: src/main.cpp include/my\\ header.h \\\n  include/cost$$.h\n";
        assert_eq!(depfile_inputs(depfile), ["src/main.cpp", "include/my header.h", "include/cost$.h"]);

        let depfile = "D:\\a\\obj\\main.cpp.o: D:\\a\\src\\main.cpp \\\r\n  C:\\Program\\ Files\\x.h\r\n";
        assert_eq!(depfile_inputs(depfile), ["D:\\a\\src\\main.cpp", "C:\\Program Files\\x.h"]);
    }

    #[test]
    fn globs_match_names_and_directories() {
        assert!(matches_glob("examples/simple.cpp", "examples/simple.cpp"));
        assert!(matches_glob("*.c", "lapi.c"));
        assert!(!matches_glob("*.c", "src/lapi.c"));
        assert!(matches_glob("**/*_test.cc", "db/db_test.cc"));
        assert!(matches_glob("**/*_test.cc", "db_test.cc"));
        assert!(matches_glob("testes/**", "testes/libs/lib1.c"));
        assert!(matches_glob("port/port_?osix.cc", "port/port_posix.cc"));
        assert!(!matches_glob("testes/**", "src/testes.c"));
        assert!(!matches_glob("src/*.cc", "src/fmt.cpp"));
    }
}
