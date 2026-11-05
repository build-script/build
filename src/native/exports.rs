// Windows exports of a shared library only what __declspec(dllexport) marks: a .def file names every
// function and variable its objects define, like WINDOWS_EXPORT_ALL_SYMBOLS of CMake
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

use crate::common::system;

/// What compilers put into every object that uses it, like string literals and floating point constants,
/// and the deleting destructors, which come with their classes
const LEFT_OUT: &[&str] = &["??_C@", "__real@", "__xmm@", "__ymm@", "__zmm@", "??_G", "??_E"];

/// The functions and the variables the objects define. Variables are DATA, programs reach those only
/// through __declspec(dllimport).
pub fn write_def_file(objects: &[PathBuf], def_file: &Path) -> Result<(), String> {
    let mut exports = BTreeMap::new();

    for object in objects {
        let data = system::read_bytes(object)?;
        let file = object::File::parse(&*data).map_err(|error| format!("{}: {error}", object.display()))?;

        for symbol in file.symbols() {
            // undefined ones have no section
            let (Some(index), Ok(name)) = (symbol.section_index(), symbol.name()) else {
                continue;
            };

            if !symbol.is_global() || name.contains('.') || LEFT_OUT.iter().any(|prefix| name.starts_with(prefix)) {
                continue;
            }

            let code = file.section_by_index(index).is_ok_and(|section| section.kind() == SectionKind::Text);
            exports.insert(name.to_string(), code);
        }
    }

    let lines: Vec<String> = exports.iter()
        .map(|(name, code)| if *code { format!("    {name}\n") } else { format!("    {name} DATA\n") })
        .collect();

    return system::write_file(def_file, &format!("EXPORTS\n{}", lines.concat()));
}

#[cfg(test)]
mod tests {
    use std::fs;

    use object::write::{self, StandardSection, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

    use crate::common::test_support::ScratchDir;

    use super::*;

    #[test]
    fn functions_and_variables_are_exported_but_string_literals_are_not() {
        let mut object = write::Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = object.section_id(StandardSection::Text);
        let data = object.section_id(StandardSection::Data);
        let read_only = object.section_id(StandardSection::ReadOnlyData);
        object.append_section_data(text, &[0xc3; 8], 1);
        object.append_section_data(data, &[0; 8], 4);
        object.append_section_data(read_only, b"hello\0", 1);

        let symbols = [
            ("twice", SymbolSection::Section(text), SymbolKind::Text, SymbolScope::Linkage),
            ("counter", SymbolSection::Section(data), SymbolKind::Data, SymbolScope::Linkage),
            ("??_C@_05CJBACGMB@hello?$AA@", SymbolSection::Section(read_only), SymbolKind::Data, SymbolScope::Linkage),
            ("??_GShape@@UEAAPEAXI@Z", SymbolSection::Section(text), SymbolKind::Text, SymbolScope::Linkage),
            ("helper", SymbolSection::Section(text), SymbolKind::Text, SymbolScope::Compilation),
            ("puts", SymbolSection::Undefined, SymbolKind::Text, SymbolScope::Linkage),
        ];

        for (name, section, kind, scope) in symbols {
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind,
                scope,
                weak: false,
                section,
                flags: SymbolFlags::None,
            });
        }

        let directory = ScratchDir::new("exports");
        let object_file = directory.join("twice.c.o");
        system::create_dir(&directory).unwrap();
        fs::write(&object_file, object.write().unwrap()).unwrap();

        let def_file = directory.join("twice.def");
        write_def_file(&[object_file], &def_file).unwrap();

        assert_eq!(system::read_file(&def_file).unwrap(), "EXPORTS\n    counter DATA\n    twice\n");
    }
}
