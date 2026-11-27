// Bundles: what one library is in every language, like Vulkan. The ports of vcpkg, which C and C++ install with their
// toolchain for their target, the Maven libraries the JVM resolves, the libraries of Go and the crates of Rust, which
// plugins take with bundled(), and modules everyone gets, like what programs need when they run. A bundle builds
// nothing: each module depending on it takes what its language takes of it.
use std::collections::HashSet;
use std::sync::Arc;

use crate::common::system;
use crate::language::runtime::{BuiltModule, FunctionParameters, ModuleOutput, Type, Value, builtin};

/// The function of a bundle, the modules a list holds are told apart by it before they are built
pub const BUNDLE_FUNCTION: &str = "bundle";

/// What a bundle holds besides its modules, which are those of its BuiltModule
#[derive(Debug, Default, Hash)]
pub struct BundleOutput {
    /// like "vulkan-loader[xcb]"
    pub ports: Vec<String>,
    /// coordinates, with a classifier for natives like org.lwjgl:lwjgl:3.4.3:natives-linux
    pub maven: Vec<String>,
    /// path@version, like github.com/bbredesen/go-vk@v1.4.366-0.4.0
    pub go: Vec<String>,
    /// name@version
    pub crates: Vec<String>,
}

/// The parts bundled() hands out, the names of the parameters of bundle()
const PARTS: [&str; 4] = ["ports", "maven", "go", "crates"];

/// A bundle with the bundles in it unpacked: their parts in one, the modules in them that are no bundles
#[derive(Debug, Default)]
pub struct Unpacked {
    pub parts: BundleOutput,
    pub modules: Vec<Arc<BuiltModule>>,
}

impl Unpacked {
    fn part(&self, part: &str) -> &[String] {
        match part {
            "ports" => return &self.parts.ports,
            "maven" => return &self.parts.maven,
            "go" => return &self.parts.go,
            _ => return &self.parts.crates,
        }
    }
}

/// Of a bundle: its parts and those of the bundles in it, each once, and the modules in them. None for other modules.
pub fn unpacked(module: &Arc<BuiltModule>) -> Option<Unpacked> {
    fn visit(module: &Arc<BuiltModule>, seen: &mut HashSet<*const BuiltModule>, unpacked: &mut Unpacked) {
        if !seen.insert(Arc::as_ptr(module)) {
            return;
        }

        let ModuleOutput::Bundle(parts) = &module.output else {
            unpacked.modules.push(module.clone());
            return;
        };

        let add = |known: &mut Vec<String>, more: &[String]| {
            for item in more {
                if !known.contains(item) {
                    known.push(item.clone());
                }
            }
        };

        add(&mut unpacked.parts.ports, &parts.ports);
        add(&mut unpacked.parts.maven, &parts.maven);
        add(&mut unpacked.parts.go, &parts.go);
        add(&mut unpacked.parts.crates, &parts.crates);

        for inner in &module.modules {
            visit(inner, seen, unpacked);
        }
    }

    if !matches!(module.output, ModuleOutput::Bundle(_)) {
        return None;
    }

    let mut unpacked = Unpacked::default();
    visit(module, &mut HashSet::new(), &mut unpacked);

    return Some(unpacked);
}

pub fn builtins() -> Vec<(&'static str, Value)> {
    let dependencies = || Type::list(Type::Union(vec![Type::String, Type::Module]));

    return vec![
        builtin(
            BUNDLE_FUNCTION,
            FunctionParameters::new()
                .required("name", Type::String)
                // installed by the C and C++ modules depending on it, for their target with their toolchain
                .strings("ports")
                // resolved by the JVM modules depending on it, natives with their classifier
                .strings("maven")
                // what the plugins of Go and Rust take with bundled()
                .strings("go")
                .strings("crates")
                // what every module depending on it depends on as well, like a prebuilt_library with the environment
                // of its programs
                .with_default("modules", Type::list(Type::Module), Value::List(vec![]))
                .returns(Type::Module),
            |context, args| {
                let name: String = args.get("name")?;
                let mut modules = vec![];

                for value in args.get::<Vec<Value>>("modules")? {
                    let Value::Module(module) = value else { continue };
                    modules.push(module.wait().map_err(|_| format!("{} it bundles failed", module.function))?);
                }

                let output = BundleOutput {
                    ports: args.get("ports")?,
                    maven: args.get("maven")?,
                    go: args.get("go")?,
                    crates: args.get("crates")?,
                };

                // what depends on it builds again when what it holds changes
                let names: Vec<&str> = modules.iter().map(|module| module.name.as_str()).collect();
                let stamp = context.build_dir(&context.project_dir).join("bundles").join(&name);
                system::write_file_if_changed(&stamp, &format!("{output:?} {names:?}"))?;

                return Ok(Value::from(Arc::new(BuiltModule {
                    name,
                    directory: context.project_dir.clone(),
                    artifact: stamp,
                    modules,
                    output: ModuleOutput::Bundle(output),
                    tests: vec![],
                })));
            },
        ),
        builtin(
            "bundled",
            FunctionParameters::new()
                .required("dependencies", dependencies())
                .required("part", Type::Union(PARTS.iter().map(|part| Type::Variant(part.to_string())).collect()))
                .returns(Type::list(Type::String)),
            // a part of the bundles among the dependencies, each item once: for "ports" also the strings of the
            // list, which a list of C and C++ dependencies names ports with
            |_, args| {
                let part: String = args.get("part")?;
                let mut found: Vec<String> = vec![];

                for value in args.get::<Vec<Value>>("dependencies")? {
                    let items = match value {
                        Value::String(port) if part == "ports" => vec![port],
                        Value::Module(module) if module.function == BUNDLE_FUNCTION => {
                            let built = module.wait().map_err(|_| format!("{} it uses failed", module.function))?;
                            unpacked(&built).map(|unpacked| unpacked.part(&part).to_vec()).unwrap_or_default()
                        }
                        _ => continue,
                    };

                    for item in items {
                        if !found.contains(&item) {
                            found.push(item);
                        }
                    }
                }

                return Ok(Value::List(found.into_iter().map(Value::String).collect()));
            },
        ),
        builtin(
            "bundled_modules",
            FunctionParameters::new()
                .required("dependencies", dependencies())
                .returns(Type::list(Type::Module)),
            // the modules among the dependencies with the bundles replaced by the modules they hold: what a function
            // of a plugin gives build_command, which takes modules that build something
            |_, args| {
                let mut modules = vec![];

                for value in args.get::<Vec<Value>>("dependencies")? {
                    let Value::Module(module) = &value else { continue };

                    if module.function != BUNDLE_FUNCTION {
                        modules.push(value);
                        continue;
                    }

                    let built = module.wait().map_err(|_| format!("{} it uses failed", module.function))?;
                    modules.extend(unpacked(&built).unwrap_or_default().modules.into_iter().map(Value::from));
                }

                return Ok(Value::List(modules));
            },
        ),
    ];
}
