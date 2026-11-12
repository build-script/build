use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::jvm::{JvmOutput, maven};
use crate::language::parser::Parser;

use super::*;

fn run(source: &str) -> Result<Runtime, RuntimeError> {
    return run_in(Runtime::new("."), source);
}

fn run_in(mut runtime: Runtime, source: &str) -> Result<Runtime, RuntimeError> {
    let script = Parser::new(source)
        .parse_script()
        .expect("script should parse");

    runtime.run(&script)?;

    // like the command line: the errors of builds count once they are done
    if let Err(mut errors) = runtime.finish() {
        return Err(errors.remove(0));
    }

    return Ok(runtime);
}

fn variable(runtime: &Runtime, name: &str) -> Value {
    return runtime.scope.lock()
        .unwrap()
        .get(name)
        .unwrap_or_else(|| panic!("{name} is not defined"));
}

fn string(runtime: &Runtime, name: &str) -> String {
    return variable(runtime, name).to_string();
}

#[test]
fn strings_take_escapes_and_must_be_closed() {
    let runtime = run(r#"x = "GPLATFORM=\"darwin\" \\ \t C:\Users""#).unwrap();
    assert_eq!(string(&runtime, "x"), "GPLATFORM=\"darwin\" \\ \t C:\\Users");
    assert_eq!(variable(&runtime, "x").literal(), r#""GPLATFORM=\"darwin\" \\ \t C:\\Users""#);

    let error = Parser::new("x = list(\"abc)").parse_script().unwrap_err();
    assert_eq!(error.variant.to_string(), "expected \" to close the string, got the end of the script");
}

#[test]
fn conditions_choose_their_block() {
    let runtime = run(r#"
        ci = "true"
        jdk = if ci == "true" { "21" } else { "17" }
        size = if 3 > 5 { "big" } else if 3 >= 3 { "same" } else { "small" }
        nothing = if false { "x" }
        both = 1 < 2 && "a" <= "b" || false
        pick = fn (ci: bool) -> str { if ci { "ci" } else { "local" } }
        local = pick(false)
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "jdk"), "21");
    assert_eq!(string(&runtime, "size"), "same");
    assert!(matches!(variable(&runtime, "nothing"), Value::Unit));
    assert_eq!(string(&runtime, "both"), "true");
    assert_eq!(string(&runtime, "local"), "local");

    // the right side only runs when the left one does not decide
    assert!(run("x = false && undefined").is_ok());
    assert!(run("x = true || undefined").is_ok());

    let error = run(r#"x = if "yes" { 1 }"#).err().unwrap();
    assert_eq!(error.to_string(), "the condition of an if must be bool, got str");
    assert_eq!(run("x = 1 && true").err().unwrap().to_string(), "&& must be bool, got int");
    assert!(matches!(
        run(r#"x = 1 < "2""#).err().unwrap().variant,
        RuntimeErrorType::InvalidBinaryOperands { .. }
    ));

    // what a block defines stays in it
    let error = run("if true { inner = 1 }\ny = inner").err().unwrap();
    assert!(matches!(error.variant, RuntimeErrorType::UndefinedVariable { .. }));
}

#[test]
fn environment_variables_with_a_default() {
    let runtime = run(r#"
        path = env("PATH")
        missing = env("BUILD_SCRIPT_SURELY_NOT_SET", default = "fallback")
        has = has_env("PATH") && !has_env("BUILD_SCRIPT_SURELY_NOT_SET")
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "path"), std::env::var("PATH").unwrap());
    assert_eq!(string(&runtime, "missing"), "fallback");
    assert_eq!(string(&runtime, "has"), "true");

    let error = run(r#"x = env("BUILD_SCRIPT_SURELY_NOT_SET")"#).err().unwrap();
    assert!(error.to_string().contains("BUILD_SCRIPT_SURELY_NOT_SET is not set"), "{error}");
}

/// A JVM library that cannot be started
fn library_output() -> ModuleOutput {
    return ModuleOutput::Jvm(JvmOutput {
        classes: None,
        libraries: vec![],
        platforms: vec![],
        android: None,
        launch: None,
        host_library_dirs: vec![],
    });
}

/// f(a: String, b?: Bool, c: String = "c", rest: String...) -> String
fn with_echo() -> Runtime {
    fn echo(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let a: String = args.get("a")?;
        let b: Option<bool> = args.get("b")?;
        let c: String = args.get("c")?;
        let rest: Vec<String> = args.rest()?;

        return Ok(format!("a={a} b={b:?} c={c} rest={rest:?}").into());
    }

    let signature = FunctionParameters::new()
        .required("a", Type::String)
        .optional("b", Type::Bool)
        .with_default("c", Type::String, "c")
        .variadic("rest", Type::String)
        .returns(Type::String);

    let mut runtime = Runtime::new(".");
    let (name, value) = builtin("f", signature, echo);
    runtime.define(name, value);

    return runtime;
}

#[test]
fn arguments_bind_positionally_by_name_and_by_default() {
    let runtime = run_in(
        with_echo(),
        r#"
        a = "outer"
        positional = f("x", true, "y", "1", "2")
        defaults = f("x")
        named = f(c = "z", a = a)
        skipped = f("x", c = "z")
        rest = f("x", rest = list("1", "2"))
        in_place = f(a = "x", b = true, "y")
    "#,
    )
    .unwrap();

    assert_eq!(
        string(&runtime, "positional"),
        r#"a=x b=Some(true) c=y rest=["1", "2"]"#
    );
    assert_eq!(string(&runtime, "defaults"), "a=x b=None c=c rest=[]");
    assert_eq!(string(&runtime, "named"), "a=outer b=None c=z rest=[]");
    assert_eq!(string(&runtime, "skipped"), "a=x b=None c=z rest=[]");
    assert_eq!(
        string(&runtime, "rest"),
        r#"a=x b=None c=c rest=["1", "2"]"#
    );
    // named arguments at their own position may be followed by positional ones
    assert_eq!(string(&runtime, "in_place"), "a=x b=Some(true) c=y rest=[]");

    // a named argument is not an assignment
    assert_eq!(string(&runtime, "a"), "outer");
}

#[test]
fn invalid_arguments_are_reported() {
    let error = |source: &str| run_in(with_echo(), source).err().expect(source).variant;

    assert!(
        matches!(error("f()"), RuntimeErrorType::MissingArgument { function, param } if function == "f" && param == "a")
    );
    assert!(
        matches!(error("f(\"x\", d = \"?\")"), RuntimeErrorType::UnknownParameter { param, .. } if param == "d")
    );
    assert!(
        matches!(error("f(\"x\", a = \"y\")"), RuntimeErrorType::DuplicateArgument { param, .. } if param == "a")
    );
    assert!(
        matches!(error("f(\"x\", true, \"c\", \"1\", rest = list())"), RuntimeErrorType::DuplicateArgument { param, .. } if param == "rest")
    );
    assert!(
        matches!(error("f(c = \"x\", true)"), RuntimeErrorType::PositionalAfterNamed { function } if function == "f")
    );
    assert!(
        matches!(error("type(\"x\", \"y\")"), RuntimeErrorType::TooManyArguments { function, max: 1, got: 2 } if function == "type")
    );

    let RuntimeErrorType::TypeMismatch {
        param,
        expected,
        got,
        ..
    } = error("f(\"x\", \"no bool\")")
    else {
        panic!()
    };

    assert_eq!(
        (param.as_str(), expected.to_string(), got.to_string()),
        ("b", "bool".to_string(), "str".to_string())
    );

    let RuntimeErrorType::TypeMismatch {
        param,
        expected,
        got,
        ..
    } = error("f(\"x\", true, \"y\", list(\"1\"))")
    else {
        panic!()
    };

    assert_eq!(
        (param.as_str(), expected.to_string(), got.to_string()),
        ("rest", "str".to_string(), "list<str>".to_string())
    );

    let RuntimeErrorType::TypeMismatch { expected, .. } = error("f(\"x\", rest = list(true))")
    else {
        panic!()
    };

    assert_eq!(expected.to_string(), "list<str>");
}

#[test]
fn builtins_must_return_their_declared_type() {
    let mut runtime = Runtime::new(".");
    let signature = FunctionParameters::new().returns(Type::String);
    let (name, value) = builtin("bad", signature, |_, _| Ok(Value::Bool(true)));
    runtime.define(name, value);

    let RuntimeErrorType::BuiltinFailed { message, .. } =
        run_in(runtime, "bad()").err().unwrap().variant
    else {
        panic!()
    };

    assert!(message.contains("returned bool, declared str"), "{message}");
}

#[test]
fn types_describe_values_and_functions() {
    let runtime = run(r#"
        use kotlin
        strings = type(list("a", "b"))
        empty = type(list())
        mixed = type(list("a", true))
        nested = type(list(list("a")))
        printer = type(print)
        builder = type(build_kotlin)
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "strings"), "list<str>");
    assert_eq!(string(&runtime, "empty"), "list<any>");
    assert_eq!(string(&runtime, "mixed"), "list<any>");
    assert_eq!(string(&runtime, "nested"), "list<list<str>>");
    assert_eq!(string(&runtime, "printer"), "fn(values: any...) -> unit");
    assert_eq!(
        string(&runtime, "builder"),
        r#"fn(name: str, dependencies: list<str | module>, repositories: list<str>, main_class: str?, fat_jar: bool = false, relocate: list<str> = [], compile_only: list<str | module> = [], plugins: list<str> = [], plugin_options: list<str> = [], kotlin_args: list<str> = [], annotation_processors: list<str | module> = [], ksp: list<str | module> = [], ksp_options: list<str> = [], ksp_version: str = "2.3.12", kotlin_version: str = "2.4.21", jdk: str = "21", directory: str = ".", source_dirs: list<str> = [src/main/kotlin, src/main/java], generated_sources: list<str> = [], resource_dirs: list<str> = [src/main/resources], resource_files: list<str> = [], resource_texts: list<str> = [], resource_tokens: list<str> = [], test_dependencies: list<str | module> = [], test_source_dirs: list<str> = [src/test/kotlin, src/test/java], test_annotation_processors: list<str | module> = [], test_ksp: list<str | module> = [], test_main_class: str?) -> module"#,
    );

    // function types compare by shape, parameter names do not matter
    let shape = |name: &str| {
        Type::Function(Arc::new(
            FunctionParameters::new()
                .required(name, Type::String)
                .returns(Type::Unit),
        ))
    };

    let optional = Type::Function(Arc::new(
        FunctionParameters::new()
            .optional("x", Type::String)
            .returns(Type::Unit),
    ));

    assert_eq!(shape("x"), shape("y"));
    assert_ne!(shape("x"), optional);
    assert!(Type::list(Type::String).accepts(&Value::List(vec![])));
    assert!(!Type::list(Type::String).accepts(&Value::List(vec![Value::Bool(true)])));
}

#[test]
fn use_kotlin_loads_build_functions() {
    let runtime = run("use kotlin\nrepos = list(maven_central(), google(),)").unwrap();

    for function in [
        "build_kotlin",
        "jdk",
        "maven_local",
        "maven",
        "platform",
        "generate_sources",
    ] {
        assert!(
            runtime.scope.lock().unwrap().get(function).is_some(),
            "{function} is missing"
        );
    }

    assert!(matches!(
        Some(variable(&runtime, "build_kotlin")),
        Some(Value::Builtin {
            name: "build_kotlin",
            ..
        })
    ));
    assert_eq!(
        variable(&runtime, "repos").to_string(),
        format!("[{}, {}]", maven::MAVEN_CENTRAL, maven::GOOGLE)
    );
}

#[test]
fn build_functions_need_use() {
    let error = run("build_kotlin(\"app\", list(), list())").err().unwrap();
    assert!(matches!(
        error.variant,
        RuntimeErrorType::UndefinedVariable { .. }
    ));

    // scripts with the names from before learn the new one
    let error = run("use kotlin\nbuildKotlin(\"app\", list(), list())")
        .err()
        .unwrap();

    assert_eq!(
        error.to_string(),
        "variable 'buildKotlin' is not defined, the function is called build_kotlin"
    );

    let error =
        run("use kotlin\nbuild_kotlin(\"app\", list(), list(), mainClass = \"MainKt\")")
            .err()
            .unwrap();

    assert_eq!(
        error.to_string(),
        "build_kotlin has no parameter 'mainClass', it is called main_class"
    );

    let error = run("use cobol").err().unwrap();
    assert!(matches!(
        error.variant,
        RuntimeErrorType::UnknownPlatform { .. }
    ));
}

#[test]
fn build_kotlin_validates_arguments() {
    let error = run("use kotlin\nbuild_kotlin(\"app\")").err().unwrap();
    assert!(
        matches!(&error.variant, RuntimeErrorType::MissingArgument { function, param } if function == "build_kotlin" && param == "dependencies")
    );

    let error = run("use kotlin\nbuild_kotlin(\"app\", \"broken\", list(maven_central()))")
        .err()
        .unwrap();

    assert_eq!(
        error.to_string(),
        "build_kotlin: argument 'dependencies' must be list<str | module>, got str"
    );
    assert_eq!((error.span.start, error.span.end), (31, 39));

    let error = run("use kotlin\nbuild_kotlin(\"app\", list(\"broken\"), list(maven_central()), jdk = \"21\")").err().unwrap();
    let RuntimeErrorType::BuiltinFailed { name, message } = error.variant else {
        panic!()
    };

    assert_eq!(name, "build_kotlin");
    assert!(message.contains("'broken'"), "{message}");
}

#[test]
fn functions_are_values_and_return_their_last_expression() {
    let runtime = run(r#"
        my_fn = fn (param1: str) -> str { param1 }
        same = my_fn("x")

        // defaults, named arguments and local variables
        greet = fn (name: str, greeting: str = "Hello") -> str {
            text = greeting + ", " + name
            text
        }
        hello = greet("Ada")
        hi = greet(greeting = "Hi", name = "Bob")

        // closures see the variables where they were defined
        prefix = "lib-"
        named = fn (name: str) -> str { prefix + name }

        // functions as arguments and as results
        apply = fn (f: fn(str) -> str, value: str) -> str { f(value) }
        applied = apply(named, "core")
        adder = fn (a: int) -> fn(int) -> int { fn (b: int) -> int { a + b } }
        five = adder(2)(3)

        // builtins inside, no declared type, nothing to return
        libraries = fn (extra: list<str>) { list("a") + extra }
        all = libraries(list("b"))
        nothing = fn () -> unit { local = 1 }
        empty = nothing()

        types = type(greet)
        applier = type(apply)
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "same"), "x");
    assert_eq!(string(&runtime, "hello"), "Hello, Ada");
    assert_eq!(string(&runtime, "hi"), "Hi, Bob");
    assert_eq!(string(&runtime, "applied"), "lib-core");
    assert_eq!(string(&runtime, "five"), "5");
    assert_eq!(string(&runtime, "all"), "[a, b]");
    assert_eq!(string(&runtime, "empty"), "()");
    assert_eq!(
        string(&runtime, "types"),
        r#"fn(name: str, greeting: str = "Hello") -> str"#
    );
    assert_eq!(
        string(&runtime, "applier"),
        "fn(f: fn(str) -> str, value: str) -> str"
    );
    assert_eq!(string(&runtime, "my_fn"), "<fn my_fn>");
    // what a call assigns stays in its scope
    assert!(
        runtime.scope.lock().unwrap().get("text").is_none()
            && runtime.scope.lock().unwrap().get("local").is_none()
    );
}

#[test]
fn functions_check_their_types() {
    let error = |source: &str| run(source).err().expect(source).variant;

    let RuntimeErrorType::ReturnTypeMismatch {
        function,
        expected,
        got,
    } = error("bad = fn () -> str { 1 }\nbad()")
    else {
        panic!()
    };

    assert_eq!(
        (function.as_str(), expected.to_string(), got.to_string()),
        ("bad", "str".to_string(), "int".to_string())
    );

    let RuntimeErrorType::TypeMismatch {
        function, param, ..
    } = error("f = fn (count: int) -> int { count }\nf(\"x\")")
    else {
        panic!()
    };

    assert_eq!((function.as_str(), param.as_str()), ("f", "count"));

    assert!(
        matches!(error("f = fn (x: text) -> str { x }"), RuntimeErrorType::UnknownType { name } if name == "text")
    );
    assert!(
        matches!(error("f = fn (x: str, x: int) { x }"), RuntimeErrorType::DuplicateParameter { param, .. } if param == "x")
    );
    assert!(matches!(
        error("f = fn (x: int = \"one\") { x }"),
        RuntimeErrorType::TypeMismatch { .. }
    ));
    assert!(
        matches!(error("f = fn (x: str) { x }\nf()"), RuntimeErrorType::MissingArgument { function, .. } if function == "f")
    );
    assert!(
        matches!(error("again = fn () -> int { again() }\nagain()"), RuntimeErrorType::CallsTooDeep { function } if function == "again")
    );

    // a function type only fits functions of that shape
    assert!(matches!(
        error("apply = fn (f: fn(str) -> str) { f(\"x\") }\napply(fn (n: int) -> int { n })"),
        RuntimeErrorType::TypeMismatch { .. }
    ));
}

#[test]
fn variants_accept_only_their_strings() {
    let pick = "pick = fn (name: str, toolchain: \"auto\" | \"local\" = \"auto\") -> str { toolchain }\n";
    let runtime = run(&format!("{pick}a = pick(\"x\")\nb = pick(\"x\", \"local\")\nc = pick(\"x\", toolchain = \"auto\")")).unwrap();
    assert_eq!((string(&runtime, "a"), string(&runtime, "b"), string(&runtime, "c")), ("auto".into(), "local".into(), "auto".into()));

    let error = |source: &str| run(&format!("{pick}{source}")).err().expect(source);
    let message = |source: &str| error(source).to_string();

    assert_eq!(message("pick(\"x\", \"remote\")"), "pick: argument 'toolchain' must be \"auto\" | \"local\", got \"remote\"");
    assert!(message("pick(\"x\", \"Local\")").ends_with("got \"Local\", did you mean \"local\"?"));
    assert!(message("pick(\"x\", \"locla\")").ends_with("did you mean \"local\"?"));
    assert!(message("pick(\"x\", 1)").ends_with("got int"));

    // a default must be one of the variants already where the function is defined
    assert!(matches!(
        error("other = fn (mode: \"a\" | \"b\" = \"c\") { mode }").variant,
        RuntimeErrorType::TypeMismatch { got, .. } if got == "\"c\""
    ));

    assert_eq!(
        message("level = fn () -> \"debug\" | \"release\" { \"test\" }\nlevel()"),
        "level must return \"debug\" | \"release\", got \"test\""
    );
    assert_eq!(
        message("modes = fn (modes: list<\"fast\" | \"small\">) { modes }\nmodes(list(\"fast\", \"smal\"))"),
        "modes: argument 'modes' must be list<\"fast\" | \"small\">, got a list with \"smal\", did you mean \"small\"?"
    );
    assert_eq!(
        message("twice = fn (mode: \"a\" | \"b\" | \"a\") { mode }"),
        "\"a\" is listed twice in \"a\" | \"b\" | \"a\""
    );
    assert_eq!(
        message("wide = fn (mode: \"a\" | str) { mode }"),
        "str already accepts \"a\", leave the variant out of \"a\" | str"
    );

    // the order of the variants does not change the shape of a function
    let runtime = run("apply = fn (f: fn(\"a\" | \"b\") -> str) -> str { f(\"a\") }\nr = apply(fn (x: \"b\" | \"a\") -> str { x })").unwrap();
    assert_eq!(string(&runtime, "r"), "a");
}

#[test]
fn builtins_take_variants() {
    let mut runtime = Runtime::new(".");
    let mode = Type::Union(vec![Type::Variant("fast".into()), Type::Variant("small".into())]);
    let signature = FunctionParameters::new()
        .with_default("mode", mode, "fast")
        .returns(Type::String);
    let (name, value) = builtin("optimize", signature, |_, args| Ok(args.get::<String>("mode")?.into()));
    runtime.define(name, value);

    let runtime = run_in(runtime, "a = optimize()\nb = optimize(\"small\")").unwrap();
    assert_eq!((string(&runtime, "a"), string(&runtime, "b")), ("fast".into(), "small".into()));

    let mut runtime = Runtime::new(".");
    let (name, value) = builtin(
        "optimize",
        FunctionParameters::new()
            .required("mode", Type::Union(vec![Type::Variant("fast".into()), Type::Variant("small".into())]))
            .returns(Type::String),
        |_, args| Ok(args.get::<String>("mode")?.into()),
    );
    runtime.define(name, value);

    let error = run_in(runtime, "optimize(\"big\")").err().unwrap();
    assert_eq!(error.to_string(), "optimize: argument 'mode' must be \"fast\" | \"small\", got \"big\"");
}

#[test]
fn lists_concatenate_and_compare() {
    let runtime = run("a = list(\"x\") + list(\"y\")\nsame = a == list(\"x\", \"y\")").unwrap();
    assert_eq!(variable(&runtime, "a").to_string(), "[x, y]");
    assert!(matches!(variable(&runtime, "same"), Value::Bool(true)));
}

#[test]
fn comments_are_ignored() {
    let runtime = run(r#"
        // a line comment
        url = "https://example.com" // after code, // in strings stays
        /* a block comment
           over two lines */
        half = 6 / 3 /* between */ + 1
        // the last line"#)
    .unwrap();

    assert_eq!(string(&runtime, "url"), "https://example.com");
    assert_eq!(string(&runtime, "half"), "3");
}

#[test]
fn numbers_and_union_types() {
    let runtime = run(r#"
        sum = 1 + 2 * 3
        negative = -4 - 1
        same = sum == 7
        sum_type = type(sum)
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "sum"), "7");
    assert_eq!(string(&runtime, "negative"), "-5");
    assert_eq!(string(&runtime, "same"), "true");
    assert_eq!(string(&runtime, "sum_type"), "int");

    let error = run("x = 9223372036854775807 + 1").err().unwrap();
    assert!(matches!(
        error.variant,
        RuntimeErrorType::InvalidArithmetic { .. }
    ));
    assert!(matches!(
        run("x = 1 / 0").err().unwrap().variant,
        RuntimeErrorType::InvalidArithmetic { .. }
    ));

    let dependency = Type::Union(vec![Type::String, Type::Module]);
    assert_eq!(dependency.to_string(), "str | module");
    assert!(dependency.accepts(&Value::from("a:b:1")));
    assert!(!dependency.accepts(&Value::Int(1)));

    let error = run("use kotlin\nbuild_kotlin(\"app\", list(true), list(maven_central()))")
        .err()
        .unwrap();

    let RuntimeErrorType::TypeMismatch { expected, got, .. } = error.variant else {
        panic!()
    };

    assert_eq!(
        (expected.to_string(), got.to_string()),
        ("list<str | module>".to_string(), "list<bool>".to_string())
    );
}

#[test]
fn modules_list_their_dependencies_once() {
    let module = |name: &str, modules: Vec<Arc<BuiltModule>>| {
        Arc::new(BuiltModule {
            name: name.to_string(),
            directory: PathBuf::from(name),
            artifact: PathBuf::from(format!("{name}.jar")),
            modules,
            output: library_output(),
            tests: vec![],
        })
    };

    let core = module("core", vec![]);
    let data = module("data", vec![core.clone()]);
    let ui = module("ui", vec![core.clone()]);

    let names: Vec<String> = BuiltModule::transitive(&[data, ui])
        .iter()
        .map(|module| module.name.clone())
        .collect();

    assert_eq!(names, ["core", "data", "ui"]);
    assert_eq!(Value::from(core).to_string(), "core.jar");
}

#[test]
fn independent_modules_build_in_parallel_after_their_dependencies() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static MOST: AtomicUsize = AtomicUsize::new(0);
    static FINISHED: Mutex<Vec<String>> = Mutex::new(vec![]);

    fn slow(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let name: String = args.get("name")?;

        for dependency in args.modules() {
            if !FINISHED.lock().unwrap().contains(&dependency.name) {
                return Err(format!("{} was not built before {name}", dependency.name));
            }
        }

        if name == "broken" {
            return Err("broken on purpose".to_string());
        }

        let running = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        MOST.fetch_max(running, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(200));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        FINISHED.lock()
            .unwrap()
            .push(name.clone());

        return Ok(Value::from(Arc::new(BuiltModule {
            artifact: PathBuf::from(format!("{name}.jar")),
            name,
            directory: PathBuf::from("."),
            modules: vec![],
            output: library_output(),
            tests: vec![],
        })));
    }

    let runtime_with_slow = || {
        let signature = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("dependencies", Type::list(Type::Module), Value::List(vec![]))
            .returns(Type::Module);
        let mut runtime = Runtime::new(".").with_jobs(2);
        let (name, value) = builtin("slow", signature, slow);
        runtime.define(name, value);

        return runtime;
    };

    let source = "a = slow(\"a\")\nb = slow(\"b\")\nc = slow(\"c\", dependencies = list(a, b))";
    run_in(runtime_with_slow(), source).unwrap();
    assert_eq!(MOST.load(Ordering::SeqCst), 2);
    assert_eq!(FINISHED.lock().unwrap().last().map(String::as_str), Some("c"));

    // a module that fails is the error, not the modules that depend on it
    let mut runtime = runtime_with_slow();
    let script = Parser::new("x = slow(\"broken\")\ny = slow(\"after\", dependencies = list(x))")
        .parse_script()
        .unwrap();
    runtime.run(&script).unwrap();
    let errors = runtime.finish()
        .err()
        .unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].to_string(), "slow: broken on purpose");
}

#[test]
fn built_modules_are_remembered_for_run() {
    fn library(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let name: String = args.get("name")?;
        return Ok(Value::from(Arc::new(BuiltModule {
            name: name.clone(),
            directory: PathBuf::from("."),
            artifact: PathBuf::from(format!("{name}.jar")),
            modules: vec![],
            output: library_output(),
            tests: vec![],
        })));
    }

    let mut runtime = Runtime::new(".");

    let signature = FunctionParameters::new()
        .required("name", Type::String)
        .returns(Type::Module);

    let (name, value) = builtin("library", signature, library);
    runtime.define(name, value);
    let runtime = run_in(runtime, "core = library(\"core\")\nlibrary(\"app\")").unwrap();

    let built = runtime.finish().unwrap();
    let names: Vec<&str> = built.iter()
        .map(|module| module.name.as_str())
        .collect();

    assert_eq!(names, ["core", "app"]);

    let error = launch(&built[0], &[]).unwrap_err();
    assert!(error.contains("core cannot be run"), "{error}");
}

/// The modules a script in `directory` built, or the first error
#[cfg(unix)]
fn build_in(directory: &std::path::Path, source: &str, tests: TestSelection) -> Result<Vec<Arc<BuiltModule>>, String> {
    let script = Parser::new(source)
        .parse_script()
        .expect("script should parse");
    let mut runtime = Runtime::new(directory).with_tests(tests);

    runtime.run(&script).map_err(|error| error.to_string())?;
    return runtime.finish().map_err(|errors| errors[0].to_string());
}

#[cfg(unix)]
#[test]
fn commands_run_again_only_when_what_they_depend_on_changed() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command");
    // the runtime takes the project directory as the system names it, /private/var for /var on macOS
    let project = system::canonical_path(&directory).unwrap();
    let file = |name: &str| project.join(name);
    system::write_file(&file("input.txt"), "a").unwrap();
    system::write_file(&file("ignored.txt"), "1").unwrap();

    let copy = "build_command(\"copy\", list(\"sh\", \"-c\", \"cat input.txt > {output}/copy.txt; echo run >> runs.log\"), \
                inputs = list(\"*.txt\"), exclude = list(\"ignored.txt\"), outputs = list(\"{output}/copy.txt\"))";
    let runs = || system::read_file(&file("runs.log")).unwrap().lines().count();

    let built = build_in(&directory, copy, TestSelection::None).unwrap();
    assert_eq!((runs(), built[0].artifact.clone()), (1, file("build/commands/copy/copy.txt")));
    assert_eq!(system::read_file(&built[0].artifact).unwrap(), "a");

    build_in(&directory, copy, TestSelection::None).unwrap();
    assert_eq!(runs(), 1, "up to date");

    system::write_file(&file("ignored.txt"), "2").unwrap();
    build_in(&directory, copy, TestSelection::None).unwrap();
    assert_eq!(runs(), 1, "an excluded file is no input");

    system::write_file(&file("input.txt"), "b").unwrap();
    build_in(&directory, copy, TestSelection::None).unwrap();
    assert_eq!(runs(), 2, "a changed input");

    // a generator built first, at its place in the command; a changed generator builds the table again
    let generator = |text: &str| {
        format!(
            "generator = build_command(\"generator\", list(\"sh\", \"-c\", \"printf '#!/bin/sh\\\\necho {text} > $1\\\\n' > {{output}}/generate; \
             chmod +x {{output}}/generate\"), outputs = list(\"{{output}}/generate\"))\n\
             build_command(\"table\", list(generator, \"{{output}}/table.h\"), outputs = list(\"{{output}}/table.h\"))"
        )
    };

    build_in(&directory, &generator("one"), TestSelection::None).unwrap();
    assert_eq!(system::read_file(&file("build/commands/table/table.h")).unwrap(), "one\n");

    build_in(&directory, &generator("two"), TestSelection::None).unwrap();
    assert_eq!(system::read_file(&file("build/commands/table/table.h")).unwrap(), "two\n");

    let error = build_in(&directory, "build_command(\"lazy\", list(\"sh\", \"-c\", \"true\"), outputs = list(\"{output}/out.txt\"))", TestSelection::None)
        .unwrap_err();
    assert!(error.ends_with("sh did not write build/commands/lazy/out.txt"), "{error}");

    let error = build_in(&directory, "build_command(\"a/b\", list(\"true\"))", TestSelection::None).unwrap_err();
    assert!(error.contains("\"a/b\" cannot name a command"), "{error}");
}

#[cfg(unix)]
#[test]
fn commands_take_an_environment_a_path_and_run_and_test() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command-environment");
    let file = |name: &str| directory.join(name);
    system::write_file(&file("tools/hello"), "#!/bin/sh\necho hello from the path > \"$1\"\n").unwrap();
    std::process::Command::new("chmod")
        .args(["+x", "tools/hello"])
        .current_dir(&*directory)
        .status()
        .unwrap();

    let source = "build_command(\"greeting\", list(\"sh\", \"-c\", \"echo $GREETING > {output}/greeting; hello {output}/hello\"), \
                  environment = list(\"GREETING=one\", \"GREETING=two\"), path = list(\"tools\"), \
                  outputs = list(\"{output}/greeting\", \"{output}/hello\"), run = list(\"sh\", \"-c\", \"exit $1\", \"sh\"))";
    let built = build_in(&directory, source, TestSelection::None).unwrap();

    assert_eq!(system::read_file(&file("build/commands/greeting/greeting")).unwrap(), "two\n", "the later entry wins");
    assert_eq!(system::read_file(&file("build/commands/greeting/hello")).unwrap(), "hello from the path\n");
    assert!(built[0].can_launch());
    assert_eq!(launch(&built[0], &["7".to_string()]), Ok(7), "run takes the arguments after --");

    let report = "<testsuite><testcase name=\\\"a\\\"/><testcase name=\\\"b\\\"><failure/></testcase></testsuite>";
    let tested = |test: &str| {
        let source = format!("build_command(\"checked\", list(\"true\"), test = list(\"sh\", \"-c\", \"{test}\"), test_timeout = 1)");
        let built = build_in(&directory, &source, TestSelection::All).unwrap();
        return built[0].tests[0].tests.run("checked");
    };

    assert_eq!(tested("true"), TestOutcome::Passed(None));
    assert_eq!(tested("true"), TestOutcome::UpToDate, "passed before");
    assert!(matches!(tested("exit 1"), TestOutcome::Failed { .. }));
    assert!(matches!(tested("sleep 5"), TestOutcome::Failed { error, .. } if error.contains("timed out")));

    let counted = tested(&format!("echo '{report}' > {{test_reports}}/results.xml; exit 1"));
    assert!(matches!(counted, TestOutcome::Failed { counts: Some(TestCounts { tests: 2, failed: 1, .. }), .. }), "{counted:?}");

    // without build test the tests are not prepared
    let built = build_in(&directory, "build_command(\"checked\", list(\"true\"), test = list(\"true\"))", TestSelection::None).unwrap();
    assert!(built[0].tests.is_empty());
}

#[test]
fn scripts_ask_for_the_system_files_and_patterns() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("prelude");
    system::write_file(&directory.join(".nvmrc"), "v22.11.0\n").unwrap();

    let source = "architecture = architecture()\n\
                  program = executable(\"go\")\n\
                  version = match(read_file(\".nvmrc\"), \"v?(\\\\d+\\\\.\\\\d+\\\\.\\\\d+)\")\n\
                  whole = match(\"go1.27.4\", \"\\\\d+\\\\.\\\\d+\")\n\
                  none = match(\"no version\", \"\\\\d+\")\n\
                  missing = read_file(\".python-version\", default = \"3.13\")";
    let runtime = run_in(Runtime::new(&*directory), source).unwrap();

    assert!(["aarch64", "x86_64"].contains(&string(&runtime, "architecture").as_str()));
    assert_eq!(string(&runtime, "program"), format!("go{}", std::env::consts::EXE_SUFFIX));
    assert_eq!(string(&runtime, "version"), "22.11.0");
    assert_eq!(string(&runtime, "whole"), "1.27");
    assert_eq!(string(&runtime, "none"), "");
    assert_eq!(string(&runtime, "missing"), "3.13");

    let message = |source: &str| run(source).err().expect(source).to_string();
    assert_eq!(message("fail(\"go has no toolchain for this system\")"), "fail: go has no toolchain for this system");
    assert!(message("match(\"x\", \"(\")").starts_with("match: ( is no regular expression"));
    assert!(message("cache_directory(\"../maven\")").contains("cannot name a directory of the cache"));
}

/// A runtime for a script in `directory` whose errors name project.build and the plugins by their paths
fn plugin_runtime(directory: &std::path::Path, source: &str) -> (Runtime, Result<(), String>) {
    let project = system::canonical_path(directory).unwrap();
    let script = Parser::new(source)
        .parse_script()
        .expect("script should parse");
    let mut runtime = Runtime::new(&project).with_script(&project.join("project.build"), source);

    let result = runtime.run(&script)
        .map_err(|error| runtime.describe(&error))
        .and_then(|_| runtime.finish().map(|_| ()).map_err(|errors| runtime.describe(&errors[0])));

    return (runtime, result);
}

#[test]
fn plugins_define_their_functions_in_a_scope_of_their_own() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("plugins");
    let write = |name: &str, content: &str| system::write_file(&directory.join(name), content).unwrap();
    write("plugins/greet.build", "use cpp\n_prefix = \"Hello, \"\ngreeting = fn (name: str) -> str { _prefix + name }\nhere = plugin_directory\nleak = fn () -> str { secret }\n");

    let (runtime, result) = plugin_runtime(&directory, "secret = \"x\"\nuse \"./plugins/greet.build\"\nuse \"./plugins/greet.build\"\nmessage = greeting(\"Go\")");
    result.unwrap();
    assert_eq!(string(&runtime, "message"), "Hello, Go");
    assert_eq!(string(&runtime, "here"), system::canonical_path(&directory.join("plugins")).unwrap().display().to_string());

    // only what it assigns, without _: neither its helpers, nor its platforms, nor plugin_directory
    for name in ["_prefix", "build_cpp", "plugin_directory"] {
        assert!(runtime.scope.lock().unwrap().get(name).is_none(), "{name}");
    }

    // a plugin never sees the variables of the script
    let (_, result) = plugin_runtime(&directory, "secret = \"x\"\nuse \"./plugins/greet.build\"\nleak()");
    assert!(result.unwrap_err().contains("secret"));

    let (_, result) = plugin_runtime(&directory, "greeting = \"mine\"\nuse \"./plugins/greet.build\"");
    assert!(result.unwrap_err().ends_with("./plugins/greet.build defines greeting, which is defined already; a plugin keeps names starting with _ to itself"));

    write("plugins/a.build", "use \"./b.build\"\n");
    write("plugins/b.build", "use \"./a.build\"\n");
    let (_, result) = plugin_runtime(&directory, "use \"./plugins/a.build\"");
    assert!(result.unwrap_err().contains("./a.build uses itself through the plugins it uses"));

    write("plugins/eager.build", "built = build_command(\"eager\", list(\"true\"))\n");
    let (_, result) = plugin_runtime(&directory, "use \"./plugins/eager.build\"");
    assert!(result.unwrap_err().contains("build_command: plugins define functions, their modules come from the scripts that call them"));

    let (_, result) = plugin_runtime(&directory, "use \"./plugins/missing.build\"");
    assert!(result.unwrap_err().contains("no plugin ./plugins/missing.build"));

    let (_, result) = plugin_runtime(&directory, "use \"someone/build-zig\"");
    assert!(result.unwrap_err().contains("someone/build-zig names a repository"));
}

#[test]
fn errors_in_plugins_name_the_script_and_the_plugin() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("plugin-errors");
    let write = |name: &str, content: &str| system::write_file(&directory.join(name), content).unwrap();
    write("plugins/tools.build", "broken = fn () -> str {\n    undefined_name\n}\nfails = fn () -> module {\n    build_command(\"fails\", list(\"no-such-program-of-build-script\"))\n}\n");
    write("plugins/bad.build", "x = (\n");

    let (_, result) = plugin_runtime(&directory, "use \"./plugins/tools.build\"\nbroken()");
    let error = result.unwrap_err();
    assert!(error.starts_with("project.build:2:1: broken: "), "{error}");
    assert!(error.ends_with("(in plugins/tools.build:2:5)"), "{error}");

    let (_, result) = plugin_runtime(&directory, "use \"./plugins/tools.build\"\n\nfails()");
    let error = result.unwrap_err();
    assert!(error.starts_with("project.build:3:1: fails: build_command: could not start no-such-program-of-build-script"), "{error}");
    assert!(error.ends_with("(in plugins/tools.build:5:5)"), "{error}");

    let (_, result) = plugin_runtime(&directory, "use \"./plugins/bad.build\"");
    assert!(result.unwrap_err().starts_with("plugins/bad.build:2:1: expected an expression"));
}
