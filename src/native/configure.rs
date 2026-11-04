// Headers CMake generates into the build: configure_file fills templates like config.h.in,
// generate_export_header writes the macros that mark what a library exports
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::system;

/// What generate_export_header writes, for a static library: nothing to export
const EXPORT_HEADER: &str = r#"#ifndef {NAME}_EXPORT_H
#define {NAME}_EXPORT_H

#ifndef {NAME}_EXPORT
#  define {NAME}_EXPORT
#endif

#ifndef {NAME}_NO_EXPORT
#  define {NAME}_NO_EXPORT
#endif

#ifndef {NAME}_DEPRECATED
#  define {NAME}_DEPRECATED __attribute__ ((__deprecated__))
#endif

#ifndef {NAME}_DEPRECATED_EXPORT
#  define {NAME}_DEPRECATED_EXPORT {NAME}_EXPORT {NAME}_DEPRECATED
#endif

#ifndef {NAME}_DEPRECATED_NO_EXPORT
#  define {NAME}_DEPRECATED_NO_EXPORT {NAME}_NO_EXPORT {NAME}_DEPRECATED
#endif

#endif
"#;

/// The headers of the templates and the export header in `generated_dir`. A header keeps its path below
/// the directory of headers its template is in: port/port_config.h.in becomes port/port_config.h.
pub fn write_headers(
    templates: &[PathBuf],
    values: &[String],
    export_name: &str,
    header_dirs: &[PathBuf],
    generated_dir: &Path,
) -> Result<(), String> {
    let values: HashMap<&str, &str> = values.iter()
        .map(|value| value.split_once('=').unwrap_or((value, "1")))
        .collect();

    let mut headers = vec![];

    for template in templates {
        let text = fs::read_to_string(template).map_err(|error| format!("{}: {error}", template.display()))?;

        let below = header_dirs.iter()
            .filter(|dir| template.starts_with(dir))
            .max_by_key(|dir| dir.components().count())
            .and_then(|dir| template.strip_prefix(dir).ok())
            .unwrap_or(template.file_name().unwrap_or_default().as_ref());

        let Some(header) = below.to_str().and_then(|below| below.strip_suffix(".in")) else {
            return Err(format!("{} is no template like config.h.in", template.display()));
        };

        headers.push((generated_dir.join(header), configured(&text, &values)));
    }

    if !export_name.is_empty() {
        let name: String = export_name.chars()
            .map(|char| if char.is_ascii_alphanumeric() { char } else { '_' })
            .collect();
        let header = generated_dir.join(format!("{}_export.h", name.to_ascii_lowercase()));
        headers.push((header, EXPORT_HEADER.replace("{NAME}", &name.to_ascii_uppercase())));
    }

    for (header, text) in headers {
        system::write_file_if_changed(&header, &text)?;
    }

    return Ok(());
}

/// Like configure_file: @NAME@ and ${NAME} become their values, #cmakedefine NAME a #define when
/// NAME is true, #cmakedefine01 NAME a #define to 1 or 0
fn configured(template: &str, values: &HashMap<&str, &str>) -> String {
    // like if() of CMake
    let is_true = |name: &str| {
        let value = values.get(name)
            .unwrap_or(&"")
            .to_ascii_uppercase();
        let false_values = ["", "0", "OFF", "NO", "FALSE", "N", "IGNORE", "NOTFOUND"];
        return !false_values.contains(&value.as_str()) && !value.ends_with("-NOTFOUND");
    };

    let lines: Vec<String> = template.lines()
        .map(|line| {
            // spaces may follow the #
            let directive = line.trim_start()
                .strip_prefix('#')
                .map(str::trim_start)
                .unwrap_or("");

            if let Some(name) = directive.strip_prefix("cmakedefine01 ").map(str::trim) {
                return format!("#define {name} {}", if is_true(name) { 1 } else { 0 });
            }

            if let Some(rest) = directive.strip_prefix("cmakedefine ").map(str::trim) {
                let (name, definition) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));

                return match is_true(name) {
                    true => format!("#define {name} {}", substituted(definition, values)).trim_end().to_string(),
                    false => format!("/* #undef {name} */"),
                };
            }

            return substituted(line, values);
        })
        .collect();

    return lines.join("\n") + "\n";
}

/// @NAME@ and ${NAME} by their values, empty when they have none
fn substituted(line: &str, values: &HashMap<&str, &str>) -> String {
    let is_name = |name: &str| !name.is_empty() && name.chars().all(|char| char.is_ascii_alphanumeric() || char == '_');
    let mut result = String::new();
    let mut rest = line;

    while let Some(start) = rest.find(['@', '$']) {
        result.push_str(&rest[..start]);
        let found = &rest[start..];

        let reference = match found.starts_with("${") {
            true => found[2..].find('}').map(|end| (&found[2..2 + end], end + 3)),
            false => found[1..].find('@').map(|end| (&found[1..1 + end], end + 2)),
        };

        match reference {
            Some((name, length)) if is_name(name) => {
                result.push_str(values.get(name).unwrap_or(&""));
                rest = &found[length..];
            }
            _ => {
                result.push_str(&found[..1]);
                rest = &found[1..];
            }
        }
    }

    result.push_str(rest);
    return result;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_configured_like_cmake_does() {
        let values = HashMap::from([("VERSION_MAJOR", "0"), ("HAVE_FDATASYNC", "1"), ("HAVE_SNAPPY", "OFF"), ("NAME", "cmark")]);
        let template = "#define VERSION ((@VERSION_MAJOR@ << 16))\n\
            #define NAME \"${NAME}\" // mail@example.com\n\
            #cmakedefine01 HAVE_FDATASYNC\n\
            #  cmakedefine01 HAVE_SNAPPY\n\
            #cmakedefine HAVE_FDATASYNC yes\n\
            #cmakedefine HAVE_ZSTD\n";

        assert_eq!(
            configured(template, &values),
            "#define VERSION ((0 << 16))\n\
            #define NAME \"cmark\" // mail@example.com\n\
            #define HAVE_FDATASYNC 1\n\
            #define HAVE_SNAPPY 0\n\
            #define HAVE_FDATASYNC yes\n\
            /* #undef HAVE_ZSTD */\n"
        );
    }

    #[test]
    fn headers_keep_their_path_below_the_headers_of_the_module() {
        let dir = std::env::temp_dir().join(format!("build-script-configure-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        system::write_file(&dir.join("port/port_config.h.in"), "#cmakedefine01 HAVE_FULLFSYNC\n").unwrap();
        system::write_file(&dir.join("src/version.h.in"), "@MAJOR@\n").unwrap();

        let templates = [dir.join("port/port_config.h.in"), dir.join("src/version.h.in")];
        let values = ["HAVE_FULLFSYNC".to_string(), "MAJOR=2".to_string()];
        let generated = dir.join("build/generated");
        write_headers(&templates, &values, "my-lib", &[dir.clone(), dir.join("src")], &generated).unwrap();

        assert_eq!(fs::read_to_string(generated.join("port/port_config.h")).unwrap(), "#define HAVE_FULLFSYNC 1\n");
        assert_eq!(fs::read_to_string(generated.join("version.h")).unwrap(), "2\n");
        assert!(fs::read_to_string(generated.join("my_lib_export.h")).unwrap().contains("#  define MY_LIB_EXPORT\n"));

        let error = write_headers(&[dir.join("src/version.h.in").with_extension("")], &[], "", &[], &generated).unwrap_err();
        assert!(error.contains("version.h"), "{error}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
