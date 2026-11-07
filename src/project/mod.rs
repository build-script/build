// New projects and modules: `build new` and `build add`, from the templates of the language
// categories, and libraries for modules: `build install`
mod install;
mod script;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::common::git;
use crate::common::settings::Settings;
use crate::common::system;
use crate::common::up_to_date;
use crate::jvm::templates;
use crate::language::runtime;
use crate::native;

pub use install::{DEPENDENCIES, DEPENDENCY_KINDS, install};

/// What a kind of module brings into a project: its files and its build call
pub struct ModuleTemplate {
    /// the platform whose `use` brings the build function
    pub platform: &'static str,
    /// what the repositories list needs for the module's dependencies
    pub repositories: &'static [&'static str],
    /// apps come after the libraries in the script, so they can depend on them
    pub is_app: bool,
    /// relative to the module directory, with their content
    pub files: Vec<(String, String)>,
    pub function: &'static str,
    /// the arguments before the repositories
    pub positional: Vec<String>,
    pub named: Vec<(&'static str, String)>,
}

pub struct NewModule {
    pub name: String,
    /// relative to the project, None for a module at its root
    pub directory: Option<String>,
    /// of its sources, and the namespace of Android modules
    pub package: String,
    /// libraries of the script it depends on: their variable and their package
    pub dependencies: Vec<(String, String)>,
}

/// What the settings file can hold: name and meaning
pub const SETTINGS: &[(&str, &str)] = &[
    (
        DOMAIN,
        "your domain, like mvmo.dev: the names of new modules start with it, Java and Kotlin packages reversed",
    ),
    (
        REMEMBER_DOMAIN,
        "never: new and add do not offer to remember the domain they asked for",
    ),
];

const DOMAIN: &str = "domain";
const REMEMBER_DOMAIN: &str = "remember_domain";

/// when there is no one to ask, like in the examples of the Java naming conventions
const DEFAULT_DOMAIN: &str = "example.com";

/// The modules each kind of project starts with, of the JVM and of C and C++
const PROJECT_KINDS: [&[(&str, &[(&str, Option<&str>)])]; 2] = [templates::PROJECT_KINDS, native::templates::PROJECT_KINDS];

pub fn project_kinds() -> Vec<&'static str> {
    return PROJECT_KINDS.iter()
        .flat_map(|kinds| kinds.iter())
        .map(|(kind, _)| *kind)
        .collect();
}

pub fn module_kinds() -> Vec<&'static str> {
    return [templates::MODULE_KINDS, native::templates::MODULE_KINDS].concat();
}

fn module_template(kind: &str, module: &NewModule) -> Option<ModuleTemplate> {
    return templates::module_template(kind, module).or_else(|| native::templates::module_template(kind, module));
}

/// A new directory with a project.build and the modules of the kind, like cargo new.
/// `package` names the sources, else the project below the domain `domain` gives.
pub fn new_project(
    kind: &str,
    path: &Path,
    package: Option<&str>,
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    let Some((_, modules)) = PROJECT_KINDS.iter()
        .flat_map(|kinds| kinds.iter())
        .find(|(known, _)| *known == kind)
    else {
        return Err(format!(
            "unknown project kind '{kind}' (known: {})",
            project_kinds().join(", ")
        ));
    };

    let is_empty_dir = fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());

    if path.exists() && !is_empty_dir {
        return Err(format!("{} already exists", path.display()));
    }

    let name = checked_name(&path.file_name().unwrap_or_default().to_string_lossy())?;

    // C and C++ have no packages, clangd keeps its index in .cache/
    let native = native::templates::PROJECT_KINDS.iter().any(|(known, _)| *known == kind);

    let base_package = new_package(package, native, &[&name], domain)?;

    let mut script = String::new();
    let ignored = if native { "build/\n.cache/\n" } else { "build/\n" };
    let mut files = vec![(PathBuf::from(".gitignore"), ignored.to_string())];

    // every module may use the libraries before it
    let mut libraries: Vec<(String, String)> = vec![];
    for (module_kind, directory) in modules.iter() {
        let module = NewModule {
            name: directory.unwrap_or(&name).to_string(),
            directory: directory.map(str::to_string),
            package: match directory {
                Some(directory) => {
                    format!("{base_package}.{}", templates::package_segment(directory))
                }
                None => base_package.clone(),
            },
            dependencies: libraries.clone(),
        };

        let template = module_template(module_kind, &module).expect("project kinds consist of module kinds");

        let variable = variable_name(&module.name);
        script = script::with_module(&script, &variable, &module, &template)?;
        let module_dir = PathBuf::from(module.directory.as_deref().unwrap_or(""));
        files.extend(
            template.files.into_iter()
                .map(|(file, content)| (module_dir.join(file), content)),
        );
        if !template.is_app {
            libraries.push((variable, module.package));
        }
    }

    files.push((PathBuf::from("project.build"), script));

    for (file, content) in &files {
        system::write_file(&path.join(file), content)?;
    }

    let package = if native { String::new() } else { format!(" with the package {base_package}") };
    println!("Created {kind} project {name} in {}{package}", path.display());

    return Ok(());
}

/// A new module in a directory next to the script, its build call in the script. Without
/// `package` its package is project and module below the domain `domain` gives.
pub fn add_module(
    kind: &str,
    name: &str,
    script_file: &Path,
    package: Option<&str>,
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    let name = checked_name(name)?;

    let project_dir = script_file.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    let module_dir = project_dir.join(&name);

    if module_dir.exists() {
        return Err(format!("{} already exists", module_dir.display()));
    }

    if !module_kinds().contains(&kind) {
        return Err(format!(
            "unknown module kind '{kind}' (known: {})",
            module_kinds().join(", ")
        ));
    }

    let native = native::templates::MODULE_KINDS.contains(&kind);

    let source = system::read_file(script_file)?;

    let project_name = fs::canonicalize(project_dir)
        .ok()
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "project".to_string());

    let package = new_package(package, native, &[&project_name, &name], domain)?;

    let module = NewModule {
        name: name.clone(),
        directory: Some(name.clone()),
        package,
        dependencies: vec![],
    };

    let template = module_template(kind, &module).expect("module kinds have templates");

    let variable = variable_name(&name);
    let script = script::with_module(&source, &variable, &module, &template)
        .map_err(|error| format!("{}: {error}", script_file.display()))?;

    for (file, content) in &template.files {
        system::write_file(&module_dir.join(file), content)?;
    }

    system::write_file(script_file, &script)?;

    let package = if native { String::new() } else { format!(" with the package {}", module.package) };
    println!("Added {kind} module {name} in {} to {}{package}", module_dir.display(), script_file.display());
    if !template.is_app {
        println!(
            "Other modules use it with {variable} in their dependencies, like list({variable})"
        );
    }

    return Ok(());
}

/// The package of new sources: `package` as given, else `names` below the domain `domain`
/// gives. C and C++ have none.
fn new_package(
    package: Option<&str>,
    native: bool,
    names: &[&str],
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    if native {
        return Ok(String::new());
    }

    if let Some(package) = package {
        return templates::checked_package(package);
    }

    let mut segments = vec![templates::package_of_domain(&domain()?)];
    segments.extend(names.iter().map(|name| templates::package_segment(name)));

    return Ok(segments.join("."));
}

/// Removes the build directories build-script wrote below the script's directory,
/// those with fingerprints in them. Build directories of other tools stay, and the files
/// git tracks in a build directory: some repositories keep their scripts there.
pub fn clean(script_file: &Path) -> Result<(), String> {
    if !script_file.is_file() {
        return Err(format!("{} does not exist", script_file.display()));
    }

    let project_dir = script_file.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut removed = vec![];
    let mut pending = vec![project_dir.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
            let name = entry.file_name();

            if !is_dir || name == "node_modules" || name == ".git" {
                continue;
            }

            // .build where a BUILD of Bazel is in the way
            let is_build_dir = name == "build" || name == ".build";

            if is_build_dir && path.join(up_to_date::FINGERPRINTS_DIR).is_dir() {
                let tracked = git::tracks_files(&path);

                match tracked {
                    true => git::remove_untracked(&path)?,
                    false => system::remove_dir(&path)?,
                }

                removed.push((path, tracked));
            } else if !is_build_dir {
                pending.push(path);
            } else if path.join("git").is_dir() {
                // checkouts of git() with modules of their own, below a build directory without one
                pending.push(path.join("git"));
            }
        }
    }

    if removed.is_empty() {
        println!("Nothing to clean below {}", project_dir.display());
    }

    removed.sort();
    for (directory, tracked) in removed {
        match tracked {
            true => println!("Removed {}, but the files git tracks there", directory.display()),
            false => println!("Removed {}", directory.display()),
        }
    }

    return Ok(());
}

/// Removes the cache: the downloads of Maven, git and vcpkg and what was made of them, the next
/// build downloads them again. The JDKs, SDKs and toolchains stay.
pub fn clean_cache() -> Result<(), String> {
    // without the slash of the empty name
    let cache: PathBuf = system::cache_dir("").components().collect();
    if !cache.is_dir() {
        println!("Nothing to clean in {}", cache.display());
        return Ok(());
    }

    let size: u64 = system::files_below(&[cache.clone()])
        .iter()
        .filter_map(|file| fs::symlink_metadata(file).ok())
        .map(|metadata| metadata.len())
        .sum();

    system::remove_dir(&cache)?;
    println!("Removed {}, {}", cache.display(), system::format_size(size));

    return Ok(());
}

/// The developer's domain new modules are named after: the remembered one, else asked
/// in a terminal and offered to remember, else example.com
pub fn domain() -> Result<String, String> {
    let mut settings = Settings::load()?;

    if let Some(domain) = settings.get(DOMAIN) {
        return checked_domain(domain)
            .map_err(|error| format!("{}: {error}", settings.file().display()));
    }

    if !io::stdin().is_terminal() {
        return Ok(DEFAULT_DOMAIN.to_string());
    }

    let domain = loop {
        let answer = system::prompt(&format!(
            "Your domain, which names start with, like mvmo.dev [{DEFAULT_DOMAIN}]: "
        ))?;

        let domain = if answer.is_empty() {
            DEFAULT_DOMAIN.to_string()
        } else {
            answer
        };

        match checked_domain(&domain) {
            Ok(domain) => break domain,
            Err(error) => println!("{error}"),
        }
    };

    if settings.get(REMEMBER_DOMAIN) != Some("never") {
        match system::prompt(&format!(
            "Remember {domain} for the next projects? (Y/n/never): "
        ))?
        .as_str()
        {
            "" | "y" | "yes" => {
                settings.set(DOMAIN, &domain)?;
                println!(
                    "Remembered in {}, build config {DOMAIN} <domain> changes it",
                    settings.file().display()
                );
            }
            "never" => {
                settings.set(REMEMBER_DOMAIN, "never")?;
                println!("Not offered again, build config {DOMAIN} <domain> remembers one");
            }
            _ => {}
        }
    }

    return Ok(domain);
}

/// The value as the settings file keeps it, checked
pub fn checked_setting(name: &str, value: &str) -> Result<String, String> {
    match name {
        DOMAIN => return checked_domain(value),
        REMEMBER_DOMAIN if value == "never" => return Ok(value.to_string()),
        REMEMBER_DOMAIN => {
            return Err(format!(
                "{REMEMBER_DOMAIN} can only be never, unset it to be offered again"
            ));
        }
        _ => {
            let known: Vec<&str> = SETTINGS.iter()
                .map(|(name, _)| *name)
                .collect();
            return Err(format!(
                "unknown setting '{name}' (known: {})",
                known.join(", ")
            ));
        }
    }
}

/// Labels of letters, digits and hyphens, separated by dots, in lower case
fn checked_domain(domain: &str) -> Result<String, String> {
    let domain = domain.to_ascii_lowercase();

    let valid = domain.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars()
                .all(|char| char.is_ascii_alphanumeric() || char == '-')
    });

    if !valid {
        return Err(format!(
            "'{domain}' is not a domain: labels of letters, digits and hyphens separated by dots, like mvmo.dev"
        ));
    }

    return Ok(domain);
}

/// Letters, digits, - and _, starting with a letter
fn checked_name(name: &str) -> Result<String, String> {
    let valid = name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && name.chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '-' || char == '_');

    if !valid {
        return Err(format!(
            "'{name}' is not a valid name: letters, digits, - and _, starting with a letter"
        ));
    }

    return Ok(name.to_string());
}

/// The variable of a module in the script: my-lib -> my_lib, builtins are not shadowed
fn variable_name(name: &str) -> String {
    let variable: String = name.chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char
            } else {
                '_'
            }
        })
        .collect();

    // keywords of the language and builtins cannot be variables of modules
    if ["use", "fn", "if", "else"].contains(&variable.as_str())
        || runtime::builtin_names().contains(&variable.as_str())
    {
        return format!("{variable}_module");
    }

    return variable;
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::common::test_support::ScratchDir;

    use super::*;

    /// what the developer would answer, without asking anyone
    fn example() -> Result<String, String> {
        return Ok("example.com".to_string());
    }

    #[test]
    fn names_become_packages_and_variables() {
        assert_eq!(checked_domain("Mvmo.DEV"), Ok("mvmo.dev".to_string()));
        assert!(checked_domain("mvmo..dev").is_err());
        assert!(checked_domain("-mvmo.dev").is_err());
        assert!(checked_domain("mvmo.dev/x").is_err());
        assert_eq!(
            checked_setting("domain", "mvmo.dev"),
            Ok("mvmo.dev".to_string())
        );
        assert!(checked_setting("remember_domain", "always").is_err());
        assert!(
            checked_setting("colour", "blue")
                .unwrap_err()
                .starts_with("unknown setting 'colour'")
        );
        assert_eq!(variable_name("my-lib"), "my_lib");
        // a module named like a builtin must not shadow it
        assert_eq!(variable_name("list"), "list_module");
        assert!(checked_name("1st").is_err());
        assert!(checked_name("a b").is_err());
    }

    #[test]
    fn clean_removes_only_what_build_script_built() {
        let root = ScratchDir::new("clean");
        system::write_file(&root.join("project.build"), "use kotlin\n").unwrap();
        system::write_file(&root.join("app/build/fingerprints/app"), "0").unwrap();
        system::write_file(&root.join("app/build/libs/app.jar"), "").unwrap();
        // what other tools built, and what the script has below node_modules
        system::write_file(&root.join("web/build/index.html"), "").unwrap();
        system::write_file(&root.join("node_modules/x/build/fingerprints/x"), "0").unwrap();

        clean(&root.join("project.build")).unwrap();
        assert!(!root.join("app/build").exists());
        assert!(root.join("app").is_dir());
        assert!(root.join("web/build/index.html").is_file());
        assert!(root.join("node_modules/x/build/fingerprints/x").is_file());
        assert!(clean(&root.join("missing.build")).unwrap_err().contains("does not exist"));

        // a repository with scripts in build/ keeps them
        system::write_file(&root.join("lib/build/fingerprints/lib"), "0").unwrap();
        system::write_file(&root.join("lib/build/cmake/setup.sh"), "").unwrap();
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(root.as_os_str()).args(args).output().unwrap();
        git(&["init", "--quiet"]);
        git(&["add", "lib/build/cmake/setup.sh"]);

        clean(&root.join("project.build")).unwrap();
        assert!(root.join("lib/build/cmake/setup.sh").is_file());
        assert!(!root.join("lib/build/fingerprints").exists());
    }

    #[test]
    fn new_projects_and_added_modules_are_written() {
        let root = ScratchDir::new("project");


        new_project("multi", &root.join("demo"), None, example).unwrap();
        let script = system::read_file(&root.join("demo/project.build")).unwrap();
        assert!(
            script.starts_with(
                "use kotlin\n\nrepositories = list(maven_central())\n\ncore = build_kotlin(\n"
            ),
            "{script}"
        );
        assert!(script.contains("app = build_kotlin(\n    \"app\",\n    list(core),\n    repositories,\n    main_class = \"com.example.demo.app.MainKt\",\n    directory = \"app\",\n)\n"), "{script}");

        let main =
            system::read_file(&root.join("demo/app/src/main/kotlin/com/example/demo/app/Main.kt"))
                .unwrap();

        assert!(
            main.contains("import com.example.demo.core.greeting"),
            "{main}"
        );
        assert!(
            root.join("demo/core/src/main/kotlin/com/example/demo/core/Greeting.kt")
                .is_file()
        );
        assert!(
            new_project("multi", &root.join("demo"), None, example)
                .unwrap_err()
                .contains("already exists")
        );

        // the domain comes from where the developer said, reversed for Java and Kotlin
        add_module(
            "android-library",
            "ui",
            &root.join("demo/project.build"),
            None,
            || Ok("mvmo.dev".to_string()),
        )
        .unwrap();
        let script = system::read_file(&root.join("demo/project.build")).unwrap();
        assert!(
            script.starts_with(
                "use kotlin\nuse android\n\nrepositories = list(google(), maven_central())\n"
            ),
            "{script}"
        );
        // a library goes before the first app, so the app can use it
        assert!(
            script.find("ui = build_android_library(").unwrap()
                < script.find("app = build_kotlin(").unwrap(),
            "{script}"
        );
        assert!(
            root.join("demo/ui/src/main/kotlin/dev/mvmo/demo/ui/Greeting.kt")
                .is_file()
        );
        assert!(
            add_module(
                "kotlin-library",
                "ui",
                &root.join("demo/project.build"),
                None,
                example
            )
            .unwrap_err()
            .contains("already exists")
        );
        assert!(
            add_module(
                "cobol",
                "legacy",
                &root.join("demo/project.build"),
                None,
                example
            )
            .unwrap_err()
            .contains("unknown module kind")
        );

        // Java modules get javac only, without Kotlin
        new_project("java", &root.join("plain"), None, example).unwrap();
        let script = system::read_file(&root.join("plain/project.build")).unwrap();
        assert!(script.starts_with("use java\n\nrepositories = list(maven_central())\n\nplain = build_java(\n    \"plain\",\n"), "{script}");
        add_module(
            "kotlin-library",
            "util",
            &root.join("plain/project.build"),
            None,
            example,
        )
        .unwrap();
        let script = system::read_file(&root.join("plain/project.build")).unwrap();
        assert!(script.starts_with("use java\nuse kotlin\n"), "{script}");

        // a package of its own needs no domain
        new_project(
            "android",
            &root.join("hello-app"),
            Some("org.acme.hello"),
            || Err("not asked".to_string()),
        )
        .unwrap();
        assert!(
            new_project("android", &root.join("bad"), Some("org.acme.new"), example)
                .unwrap_err()
                .contains("not a package name")
        );
        let script = system::read_file(&root.join("hello-app/project.build")).unwrap();
        // Compose, without layouts and other resources
        assert!(script.contains("hello_app = build_android(\n    \"hello-app\",\n    \"org.acme.hello\",\n    list(\n        platform(\"androidx.compose:compose-bom:"), "{script}");
        assert!(script.contains("        \"androidx.activity:activity-compose:"), "{script}");
        assert!(script.ends_with("    ),\n    repositories,\n    plugins = list(\"compose\"),\n)\n"), "{script}");
        let activity = system::read_file(
            &root.join("hello-app/src/main/kotlin/org/acme/hello/MainActivity.kt"),
        )
        .unwrap();
        assert!(activity.contains("import org.acme.hello.ui.theme.AppTheme"), "{activity}");
        assert!(activity.contains("Greeting(\"hello-app\", Modifier.padding(innerPadding))"), "{activity}");
        assert!(root.join("hello-app/src/main/kotlin/org/acme/hello/ui/theme/Theme.kt").is_file());
        assert!(!root.join("hello-app/src/main/res").exists());
    }

    #[test]
    fn c_and_cpp_projects_need_no_domain_and_no_repositories() {
        let root = ScratchDir::new("native-project");
        let not_asked = || Err("not asked".to_string());

        new_project("cpp", &root.join("hello"), None, not_asked).unwrap();
        let script_file = root.join("hello/project.build");
        add_module("cpp-library", "my-lib", &script_file, None, not_asked).unwrap();

        // the library goes before the program, which is an app with program = true
        let script = system::read_file(&script_file).unwrap();
        assert_eq!(
            script,
            "use cpp\n\nmy_lib = build_cpp(\n    \"my-lib\",\n    list(),\n    directory = \"my-lib\",\n)\n\nhello = build_cpp(\n    \"hello\",\n    list(),\n    program = true,\n)\n"
        );

        let header = system::read_file(&root.join("hello/my-lib/include/my-lib/greeting.hpp")).unwrap();
        assert!(header.contains("namespace my_lib {"), "{header}");
        assert!(root.join("hello/src/main.cpp").is_file());
        assert_eq!(system::read_file(&root.join("hello/.gitignore")).unwrap(), "build/\n.cache/\n");

        // a C library brings use c and build_c
        add_module("c-library", "codec", &script_file, None, not_asked).unwrap();
        let script = system::read_file(&script_file).unwrap();
        assert!(script.starts_with("use cpp\nuse c\n"), "{script}");
        assert!(script.contains("codec = build_c(\n    \"codec\",\n"), "{script}");
    }
}
