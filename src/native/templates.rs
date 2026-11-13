// Templates of C and C++ modules for `build new` and `build add`: programs and static
// libraries, whose headers are in include/<name>/
use crate::project::{ModuleTemplate, NewModule};

pub const MODULE_KINDS: &[&str] = &["cpp-app", "cpp-library", "c-app", "c-library"];

/// What each kind of project and module is, for the choice of new and add
pub const ABOUT: &[(&str, &str)] = &[
    ("cpp", "a C++ program"),
    ("c", "a C program"),
    ("cpp-app", "a C++ program"),
    ("cpp-library", "a C++ library"),
    ("c-app", "a C program"),
    ("c-library", "a C library"),
];

/// Like those of the JVM: the kind of each module and its directory, None for the project root
pub const PROJECT_KINDS: &[(&str, &[(&str, Option<&str>)])] =
    &[("cpp", &[("cpp-app", None)]), ("c", &[("c-app", None)])];

pub fn module_template(kind: &str, module: &NewModule) -> Option<ModuleTemplate> {
    let name = &module.name;

    // my-lib -> my_lib, for namespaces and C prefixes
    let identifier: String =
        name.chars()
            .map(|char| if char.is_ascii_alphanumeric() { char } else { '_' })
            .collect();

    let (is_app, files) = match kind {
        "cpp-app" => (true, vec![("src/main.cpp".to_string(), CPP_MAIN.replace("{name}", name))]),
        "c-app" => (true, vec![("src/main.c".to_string(), C_MAIN.replace("{name}", name))]),
        "cpp-library" => (
            false,
            vec![
                (format!("include/{name}/greeting.hpp"), CPP_HEADER.replace("{identifier}", &identifier)),
                (
                    "src/greeting.cpp".to_string(),
                    CPP_GREETING.replace("{name}", name).replace("{identifier}", &identifier),
                ),
            ],
        ),
        "c-library" => (
            false,
            vec![
                (format!("include/{name}/greeting.h"), C_HEADER.replace("{name}", name).replace("{identifier}", &identifier)),
                ("src/greeting.c".to_string(), C_GREETING.replace("{name}", name).replace("{identifier}", &identifier)),
            ],
        ),
        _ => return None,
    };

    let modules: Vec<&str> = module.dependencies.iter()
        .map(|(variable, _)| variable.as_str())
        .collect();
    let (platform, function) = if kind.starts_with("cpp-") { ("cpp", "build_cpp") } else { ("c", "build_c") };

    return Some(ModuleTemplate {
        use_argument: platform.to_string(),
        // the ports come from vcpkg
        repositories: &[],
        is_app,
        files,
        function: function.to_string(),
        positional: vec![format!("\"{name}\""), format!("list({})", modules.join(", "))],
        named: match is_app {
            true => vec![("program".to_string(), "true".to_string())],
            false => vec![],
        },
    });
}

const CPP_MAIN: &str = r#"#include <iostream>

int main() {
    std::cout << "Hello from {name}\n";
    return 0;
}
"#;

const C_MAIN: &str = r#"#include <stdio.h>

int main(void) {
    printf("Hello from {name}\n");
    return 0;
}
"#;

const CPP_HEADER: &str = r#"#pragma once

#include <string>

namespace {identifier} {

std::string greeting(const std::string& name);

}
"#;

const CPP_GREETING: &str = r#"#include "{name}/greeting.hpp"

namespace {identifier} {

std::string greeting(const std::string& name) {
    return "Hello, " + name + ", from {name}";
}

}
"#;

const C_HEADER: &str = r#"#pragma once

/* "Hello from {name}" */
const char *{identifier}_greeting(void);
"#;

const C_GREETING: &str = r#"#include "{name}/greeting.h"

const char *{identifier}_greeting(void) {
    return "Hello from {name}";
}
"#;
