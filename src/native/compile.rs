// Compiling C and C++: every source on its own, in parallel, and only when its object is older than
// the source, one of its headers, its command line or the version of its compiler. clang and gcc name
// the headers in a depfile. Headers every source needs may be precompiled once before them.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::common::semaphore::Semaphore;
use crate::common::target::OperatingSystem;
use crate::common::up_to_date::hash_hex;
use crate::common::{system, terminal};

use super::toolchain::{Kind, Toolchain};

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
    /// build/ of the module, build/<target>/ for another target than the machine
    pub build_dir: &'a Path,
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
    /// like "<vector>" or "src/common.hpp": precompiled once, then the sources of the module's language
    /// take them before their first line
    pub precompiled_headers: &'a [String],
}

/// How a compiler builds and takes precompiled headers: clang, also that of Apple, zig and the NDK, a .pch
/// it is given, GCC a .gch it finds next to the header
#[derive(Debug, Clone, Copy, PartialEq)]
enum Family {
    Clang,
    Gcc,
}

impl Family {
    /// Of what the compiler says to --version
    fn of(version: &str) -> Family {
        match version.contains("clang") {
            true => return Family::Clang,
            false => return Family::Gcc,
        }
    }
}

/// Where the precompiled headers of a compilation go: the header that includes them, which clangd reads, and
/// apart from it the file the compiler made of it, which another clang or GCC could not read
struct Precompiled {
    /// <objects>/pch
    dir: PathBuf,
    /// pch.hpp or pch.h
    name: &'static str,
}

impl Precompiled {
    fn header(&self) -> PathBuf {
        return self.dir.join(self.name);
    }

    /// What the compiler translates: GCC finds its .gch only next to the header
    fn input(&self, family: Family) -> PathBuf {
        match family {
            Family::Clang => return self.header(),
            Family::Gcc => return self.dir.join("gcc").join(self.name),
        }
    }

    fn binary(&self, family: Family) -> PathBuf {
        match family {
            Family::Clang => return self.dir.join("clang").join(format!("{}.pch", self.name)),
            Family::Gcc => return self.dir.join("gcc").join(format!("{}.gch", self.name)),
        }
    }

    /// What the sources take. GCC would quietly parse the header itself with a .gch it cannot use, it says so.
    fn arguments(&self, family: Family) -> Vec<String> {
        match family {
            Family::Clang => return vec!["-include-pch".to_string(), self.binary(family).display().to_string()],
            Family::Gcc => return vec!["-Winvalid-pch".to_string(), "-include".to_string(), self.input(family).display().to_string()],
        }
    }
}

/// The header that includes them: <vector> as it is, the module's own by their path
fn precompiled_contents(headers: &[String], module_dir: &Path) -> String {
    return headers.iter()
        .map(|header| match header.starts_with('<') {
            true => format!("#include {header}\n"),
            false => format!("#include \"{}\"\n", module_dir.join(header).display().to_string().replace('\\', "/")),
        })
        .collect();
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
        let precompiled = self.precompiled_of(source)
            .map(|precompiled| precompiled.arguments(self.family(source)))
            .unwrap_or_default();
        return self.line_with(source, precompiled);
    }

    /// What clangd gets: the header of the precompiled headers, not the file the compiler made of it
    fn editor_line(&self, source: &Path) -> Vec<String> {
        let precompiled = self.precompiled_of(source)
            .map(|precompiled| vec!["-include".to_string(), precompiled.header().display().to_string()])
            .unwrap_or_default();
        return self.line_with(source, precompiled);
    }

    fn line_with(&self, source: &Path, precompiled: Vec<String>) -> Vec<String> {
        let mut line = self.compiler(source).to_vec();
        line.push(self.standard(source));
        line.extend(self.flags.iter().cloned());
        line.extend(precompiled);
        line.extend(["-c".to_string(), source.display().to_string()]);
        line.extend(["-o".to_string(), self.object(source).display().to_string()]);

        return line;
    }

    fn standard(&self, source: &Path) -> String {
        match source.extension().is_some_and(|extension| extension == "c") {
            true => return format!("-std={}", self.c_standard),
            false => return format!("-std={}", self.cpp_standard),
        }
    }

    fn family(&self, source: &Path) -> Family {
        return Family::of(&self.toolchain.version(self.compiler(source)));
    }

    /// The precompiled headers a source takes: those of the sources in the module's language, C sources of
    /// C++ modules take none
    fn precompiled_of(&self, source: &Path) -> Option<Precompiled> {
        let is_c = source.extension().is_some_and(|extension| extension == "c");

        if self.precompiled_headers.is_empty() || is_c != (self.language == Language::C) {
            return None;
        }

        let name = match self.language {
            Language::C => "pch.h",
            Language::Cpp => "pch.hpp",
        };
        return Some(Precompiled { dir: self.objects_dir.join("pch"), name });
    }

    /// Translates the precompiled headers once their header, a header that one includes, the flags or the
    /// compiler changed. Before the sources, which compile again then.
    fn precompile(&self, sources: &[PathBuf]) -> Result<(), String> {
        let Some((source, precompiled)) = sources.iter().find_map(|source| self.precompiled_of(source).map(|precompiled| (source, precompiled))) else {
            return Ok(());
        };

        for header in self.precompiled_headers {
            if header.starts_with('<') != header.ends_with('>') {
                return Err(format!("precompiled_headers: {header} is neither <header> of the system nor a path below the module"));
            }
            if !header.starts_with('<') && !self.module_dir.join(header).is_file() {
                return Err(format!("precompiled_headers: there is no {header} below {}", self.module_dir.display()));
            }
        }

        let family = self.family(source);
        let compiler = self.compiler(source);
        let binary = precompiled.binary(family);

        // written only when they change: a new header would compile everything again
        let contents = precompiled_contents(self.precompiled_headers, self.module_dir);
        for header in [precompiled.header(), precompiled.input(family)] {
            if fs::read_to_string(&header).ok().as_deref() != Some(contents.as_str()) {
                system::write_file(&header, &contents)?;
            }
        }

        let kind = match self.language {
            Language::C => "c-header",
            Language::Cpp => "c++-header",
        };
        let mut line = compiler.to_vec();
        line.push(self.standard(source));
        line.extend(self.flags.iter().cloned());
        line.extend(["-x".to_string(), kind.to_string(), precompiled.input(family).display().to_string()]);
        line.extend(["-o".to_string(), binary.display().to_string()]);

        let recorded = RecordedCommand::new(binary.clone(), [vec![self.toolchain.version(compiler)], line.clone()].concat());
        if recorded.is_up_to_date() {
            return Ok(());
        }

        let tests = if self.tests { " of the tests" } else { "" };
        system::log(self.module_name, &format!("Precompiling {} header(s){tests}", self.precompiled_headers.len()));

        system::create_parent_dir(&binary)?;
        let mut arguments = line[compiler.len()..].to_vec();
        arguments.extend(["-MMD".to_string(), "-MF".to_string(), recorded.depfile.display().to_string()]);
        if terminal::colors() {
            arguments.push("-fdiagnostics-color=always".to_string());
        }

        let response_file = PathBuf::from(format!("{}.rsp", binary.display()));
        let mut command = self.toolchain.command_with_arguments(compiler, &arguments, &response_file)?;
        command.current_dir(self.module_dir);

        let _compiler = COMPILERS.acquire();
        system::run_command_in_one_piece(&mut command, "precompiling headers")?;

        return recorded.record();
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
        self.precompile(&sources)?;

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
        let object = self.object(source);

        if !RecordedCommand::new(object.clone(), self.recorded_line(source)).is_up_to_date() {
            return false;
        }

        // the depfile names the header of the precompiled headers, not the headers it includes
        let Some(precompiled) = self.precompiled_of(source) else {
            return true;
        };
        let modified = |path: &Path| fs::metadata(path).and_then(|metadata| metadata.modified());

        return match (modified(&precompiled.binary(self.family(source))), modified(&object)) {
            (Ok(precompiled), Ok(compiled)) => precompiled <= compiled,
            _ => false,
        };
    }

    /// For clangd, which finds build/compile_commands.json of the module from its sources
    pub fn compile_commands(&self) -> Vec<serde_json::Value> {
        return self.sources()
            .iter()
            .map(|source| {
                serde_json::json!({
                    "directory": self.module_dir,
                    "file": source,
                    "arguments": self.editor_line(source),
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
pub fn depfile_inputs(depfile: &str) -> Vec<String> {
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

    if !cpp && toolchain.target.os.links_math_library() {
        arguments.push("-lm".to_string());
    }

    // like CMake and Visual Studio every program links the libraries of Windows. link.exe takes a program
    // for the console only by a main() among the objects, that of gtest_main is in a library.
    arguments.extend(toolchain.target.os.system_libraries().iter().map(|library| format!("-l{library}")));

    arguments.extend(runtime_flags(toolchain));

    let windows_program = toolchain.target.os == OperatingSystem::Windows && program.extension().is_some_and(|extension| extension == "exe");
    if windows_program && !toolchain.mingw() {
        arguments.push("-Wl,-subsystem:console".to_string());
    }

    let linker = if cpp { &toolchain.cpp_compiler } else { &toolchain.c_compiler };
    let response_file = PathBuf::from(format!("{}.rsp", program.display()));
    let mut command = toolchain.command_with_arguments(linker, &arguments, &response_file)?;
    return system::run_command_in_one_piece(&mut command, "linking");
}

/// How the GCC build-script keeps links its runtime: inside, it is newer than that of the systems, what it builds
/// runs without that GCC. In C++26 its headers define the copy and move members of std::logic_error and
/// std::runtime_error inline, which libstdc++.a defines too: on ELF and Mach-O the inline ones are weak, on Windows
/// the linker takes them for a second definition, GCC bug 125151. Both come from the same source, the linker keeps
/// the first, that of the objects. To go with the GCC that fixes it.
fn runtime_flags(toolchain: &Toolchain) -> Vec<String> {
    if toolchain.kind != Kind::Gcc {
        return vec![];
    }

    let mut flags = vec!["-static-libgcc".to_string(), "-static-libstdc++".to_string()];

    if toolchain.target.os == OperatingSystem::Windows {
        flags.push("-Wl,--allow-multiple-definition".to_string());
    }

    return flags;
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
    use std::path::{Path, PathBuf};

    use super::{Family, Precompiled, Toolchain, depfile_inputs, linked_once, precompiled_contents, runtime_flags};

    #[test]
    fn clang_takes_its_pch_and_gcc_finds_its_gch_next_to_the_header() {
        assert_eq!(Family::of("Apple clang version 16.0.0 (clang-1600.0.26.6)"), Family::Clang);
        assert_eq!(Family::of("clang version 20.1.2"), Family::Clang);
        assert_eq!(Family::of("g++ (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0"), Family::Gcc);

        // the separators of the system the tests run on
        let dir = Path::new("/m/build/obj/m/debug/pch");
        let text = |path: PathBuf| path.display().to_string();
        let precompiled = Precompiled { dir: dir.to_path_buf(), name: "pch.hpp" };
        assert_eq!(precompiled.arguments(Family::Clang), ["-include-pch".to_string(), text(dir.join("clang").join("pch.hpp.pch"))]);
        assert_eq!(precompiled.arguments(Family::Gcc), ["-Winvalid-pch".to_string(), "-include".to_string(), text(dir.join("gcc").join("pch.hpp"))]);
        assert_eq!(precompiled.binary(Family::Gcc), dir.join("gcc").join("pch.hpp.gch"));

        // clangd reads the header, the file of another compiler is never next to it
        assert_eq!(precompiled.header(), dir.join("pch.hpp"));
        let headers = ["<vector>", "src/common.hpp"].map(String::from);
        assert_eq!(precompiled_contents(&headers, Path::new("/m")), "#include <vector>\n#include \"/m/src/common.hpp\"\n");
    }

    #[test]
    fn gcc_links_its_runtime_inside_and_on_windows_takes_the_inline_exceptions_of_cpp26() {
        use crate::common::target::Target;
        use super::Kind;

        let toolchain = |kind: Kind, target: &str| Toolchain {
            kind,
            target: Target::named(target).unwrap(),
            c_compiler: vec![],
            cpp_compiler: vec![],
            archiver: vec![],
            environment: vec![],
        };

        assert_eq!(runtime_flags(&toolchain(Kind::Gcc, "linux-x86_64")), ["-static-libgcc", "-static-libstdc++"]);
        assert_eq!(
            runtime_flags(&toolchain(Kind::Gcc, "windows-x86_64")),
            ["-static-libgcc", "-static-libstdc++", "-Wl,--allow-multiple-definition"]
        );
        // zig and the compilers of the system link their own runtime as they do
        assert!(runtime_flags(&toolchain(Kind::Zig, "windows-x86_64")).is_empty());
        assert!(runtime_flags(&toolchain(Kind::System, "linux-x86_64")).is_empty());
    }

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

/// With the compilers of the system, which the machines of the pipeline for Linux and macOS have
#[cfg(all(test, unix))]
mod with_a_compiler {
    use std::fs;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    use super::{Compilation, Language};
    use crate::common::system;
    use crate::common::test_support::ScratchDir;
    use crate::native::toolchain::Toolchain;

    /// Sets the times a build compares
    fn modified_at(file: &Path, time: SystemTime) {
        fs::File::options().write(true).open(file).unwrap().set_modified(time).unwrap();
    }

    fn modified(file: &Path) -> SystemTime {
        return fs::metadata(file).unwrap().modified().unwrap();
    }

    #[test]
    fn sources_compile_again_when_a_precompiled_header_changed() {
        let directory = ScratchDir::new("precompiled");
        let module = system::canonical_path(&directory).unwrap();
        let file = |path: &str| module.join(path);

        let long_ago = SystemTime::now() - Duration::from_secs(120);
        for (path, text) in [
            ("src/common.hpp", "#pragma once\n#include <string>\ninline int common() { return 1; }\n"),
            ("src/a.cpp", "int a() { return common() + int(std::string(\"a\").size()); }\n"),
            ("src/b.cpp", "int b() { return common(); }\n"),
            ("src/plain.c", "int plain(void) { return 0; }\n"),
        ] {
            system::write_file(&file(path), text).unwrap();
            modified_at(&file(path), long_ago);
        }

        let toolchain = Toolchain::named("system").unwrap();
        let headers = ["<vector>", "src/common.hpp"].map(String::from);
        let patterns = ["src/**".to_string()];
        let build_dir = file("build");
        let compilation = Compilation {
            module_name: "m",
            language: Language::Cpp,
            toolchain: &toolchain,
            module_dir: &module,
            build_dir: &build_dir,
            source_patterns: &patterns,
            exclude_patterns: &[],
            flags: vec![],
            c_standard: "c17",
            cpp_standard: "c++20",
            objects_dir: build_dir.join("obj/m/debug"),
            tests: false,
            candidates: OnceLock::new(),
            generated_sources: vec![],
            precompiled_headers: &headers,
        };

        // the sources do not include common.hpp, they get it from the precompiled headers
        let (objects, compiled) = compilation.run().unwrap();
        assert!(compiled);
        let [a, b, plain] = ["src/a.cpp", "src/b.cpp", "src/plain.c"].map(|source| compilation.object(&file(source)));
        assert!(objects.contains(&a) && objects.contains(&plain));

        let precompiled = compilation.precompiled_of(&file("src/a.cpp")).unwrap();
        let binary = precompiled.binary(compilation.family(&file("src/a.cpp")));
        assert!(binary.is_file(), "{} is missing", binary.display());
        assert!(compilation.precompiled_of(&file("src/plain.c")).is_none(), "C sources of a C++ module take none");
        assert!(!compilation.run().unwrap().1, "nothing changed");

        // a header among them: they are translated again and the sources that take them compile again
        let a_while_ago = SystemTime::now() - Duration::from_secs(60);
        for output in [&binary, &a, &b, &plain] {
            modified_at(output, a_while_ago);
        }
        system::write_file(&file("src/common.hpp"), "#pragma once\n#include <string>\ninline int common() { return 2; }\n").unwrap();

        assert!(compilation.run().unwrap().1);
        assert!(modified(&binary) > a_while_ago && modified(&a) > a_while_ago && modified(&b) > a_while_ago);
        assert_eq!(modified(&plain), a_while_ago, "it takes no precompiled headers");
        assert!(!compilation.run().unwrap().1, "up to date again");

        // clangd gets the header, never the file of the compiler
        let commands = compilation.compile_commands();
        let arguments = commands.iter()
            .find(|command| command["file"] == file("src/a.cpp").display().to_string())
            .map(|command| command["arguments"].to_string())
            .unwrap();
        assert!(arguments.contains(&precompiled.header().display().to_string()), "{arguments}");
        assert!(!arguments.contains(".pch") && !arguments.contains(".gch") && !arguments.contains("gcc/"), "{arguments}");
    }

}
