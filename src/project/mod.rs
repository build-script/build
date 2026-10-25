// New projects and modules: `build-script new` and `build-script add`, from the templates of the language
// categories, and libraries for modules: `build-script install`
mod install;
mod script;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::common::settings::Settings;
use crate::common::system;
use crate::jvm::templates;
pub use crate::jvm::templates::MODULE_KINDS;
use crate::language::runtime;

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

pub fn project_kinds() -> Vec<&'static str> {
    return templates::PROJECT_KINDS.iter()
        .map(|(kind, _)| *kind)
        .collect();
}

/// A new directory with a project.build and the modules of the kind, like cargo new.
/// `package` names the sources, else the project below the domain `domain` gives.
pub fn new_project(
    kind: &str,
    path: &Path,
    package: Option<&str>,
    domain: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    let Some((_, modules)) = templates::PROJECT_KINDS.iter()
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

    let base_package = match package {
        Some(package) => templates::checked_package(package)?,
        None => format!(
            "{}.{}",
            templates::package_of_domain(&domain()?),
            templates::package_segment(&name)
        ),
    };

    let mut script = String::new();
    let mut files = vec![(PathBuf::from(".gitignore"), "build/\n".to_string())];

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

        let template = templates::module_template(module_kind, &module)
            .expect("project kinds consist of module kinds");

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

    println!(
        "Created {kind} project {name} in {} with the package {base_package}",
        path.display()
    );

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

    if !MODULE_KINDS.contains(&kind) {
        return Err(format!(
            "unknown module kind '{kind}' (known: {})",
            MODULE_KINDS.join(", ")
        ));
    }

    let source = system::read_file(script_file)?;

    let project_name = fs::canonicalize(project_dir)
        .ok()
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "project".to_string());

    let package = match package {
        Some(package) => templates::checked_package(package)?,
        None => {
            let prefix = templates::package_of_domain(&domain()?);
            format!(
                "{prefix}.{}.{}",
                templates::package_segment(&project_name),
                templates::package_segment(&name)
            )
        }
    };

    let module = NewModule {
        name: name.clone(),
        directory: Some(name.clone()),
        package,
        dependencies: vec![],
    };

    let template = templates::module_template(kind, &module).expect("module kinds have templates");

    let variable = variable_name(&name);
    let script = script::with_module(&source, &variable, &module, &template)
        .map_err(|error| format!("{}: {error}", script_file.display()))?;

    for (file, content) in &template.files {
        system::write_file(&module_dir.join(file), content)?;
    }

    system::write_file(script_file, &script)?;

    println!(
        "Added {kind} module {name} in {} to {} with the package {}",
        module_dir.display(),
        script_file.display(),
        module.package
    );
    if !template.is_app {
        println!(
            "Other modules use it with {variable} in their dependencies, like list({variable})"
        );
    }

    return Ok(());
}

/// Removes the build directories build-script wrote below the script's directory,
/// those with fingerprints in them. Build directories of other tools stay.
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

            if name == "build" && path.join("fingerprints").is_dir() {
                system::remove_dir(&path)?;
                removed.push(path);
            } else if name != "build" {
                pending.push(path);
            }
        }
    }

    if removed.is_empty() {
        println!("Nothing to clean below {}", project_dir.display());
    }

    removed.sort();
    for directory in removed {
        println!("Removed {}", directory.display());
    }

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
                    "Remembered in {}, build-script config {DOMAIN} <domain> changes it",
                    settings.file().display()
                );
            }
            "never" => {
                settings.set(REMEMBER_DOMAIN, "never")?;
                println!("Not offered again, build-script config {DOMAIN} <domain> remembers one");
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
        let root = std::env::temp_dir().join(format!("build-script-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
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
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn new_projects_and_added_modules_are_written() {
        let root =
            std::env::temp_dir().join(format!("build-script-project-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);

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
        fs::remove_dir_all(&root).unwrap();
    }
}
