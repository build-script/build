use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::common::runner::Runner;
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
        environment: vec![],
        jvm_flags: vec![],
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

    // neither a platform nor in the official list, which is not there either
    let runtime = Runtime::new(".").with_official_plugins("file:///nonexistent/build-script-plugins");
    let error = run_in(runtime, "use cobol").err().unwrap();
    assert!(matches!(&error.variant, RuntimeErrorType::UnknownPlugin { name, .. } if name == "cobol"));
    assert!(error.to_string().starts_with("cobol is neither a platform nor an official plugin (platforms: kotlin"), "{error}");
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

    let error = launch(&built[0], &[], &Runner::direct()).unwrap_err();
    assert!(error.contains("core cannot be run"), "{error}");
}

#[test]
fn scripts_ask_for_their_target_and_for_the_machine() {
    let host = Target::host();
    let source = "t = target()\nos = operating_system()\narch = architecture()\nexe = executable(\"app\")\n\
                  host = host_target()\nhost_os = host_operating_system()\nhost_exe = host_executable(\"app\")";

    let runtime = run(source).unwrap();
    assert_eq!((string(&runtime, "t"), string(&runtime, "host")), (host.name(), host.name()));
    assert_eq!(string(&runtime, "os"), std::env::consts::OS);
    assert_eq!(string(&runtime, "exe"), format!("app{}", std::env::consts::EXE_SUFFIX));

    // built for another system: what the script builds names that one, the machine stays the host
    let windows = Target::named("windows-x86_64").unwrap();
    let other = if host == windows { Target::named("linux-aarch64").unwrap() } else { windows };
    let runtime = run_in(Runtime::new(".").with_target(Some(other)), source).unwrap();

    assert_eq!(string(&runtime, "t"), other.name());
    assert_eq!((string(&runtime, "os"), string(&runtime, "arch")), (other.os.name().to_string(), other.arch.name().to_string()));
    assert_eq!(string(&runtime, "exe"), other.os.executable("app"));
    assert_eq!(string(&runtime, "host"), host.name());
    assert_eq!(string(&runtime, "host_os"), std::env::consts::OS);
    assert_eq!(string(&runtime, "host_exe"), format!("app{}", std::env::consts::EXE_SUFFIX));

    // build/<target>/ beside build/, and the name of the machine is the machine
    assert_eq!(runtime.context.build_dir(std::path::Path::new("app")), PathBuf::from("app/build").join(other.name()));
    assert_eq!(Runtime::new(".").with_target(Some(host)).context.target, None);

    // build --release, which plugins ask for
    let mode = "mode = if release() { \"release\" } else { \"debug\" }";
    assert_eq!(string(&run(mode).unwrap(), "mode"), "debug");
    assert_eq!(string(&run_in(Runtime::new(".").with_release(true), mode).unwrap(), "mode"), "release");
}

/// A module that finished building with the given name
fn finished_module(name: &str) -> Arc<BuiltModule> {
    return Arc::new(BuiltModule {
        name: name.to_string(),
        directory: PathBuf::from("."),
        artifact: PathBuf::from(format!("{name}.jar")),
        modules: vec![],
        output: library_output(),
        tests: vec![],
    });
}

/// A module of the script that records its name in `built` once its build runs
fn recording_module(
    name: &str,
    dependencies: Vec<Arc<PendingModule>>,
    built: &Arc<Mutex<Vec<String>>>,
) -> Arc<PendingModule> {
    let built = built.clone();
    let module_name = name.to_string();

    return PendingModule::new(ModuleCall::named("library", name), dependencies, move |module| {
        built.lock().unwrap().push(module_name.clone());
        module.finish(Ok(finished_module(&module_name)));
    });
}

#[test]
fn modules_build_only_once_requested_and_with_their_dependencies() {
    let built = Arc::new(Mutex::new(vec![]));
    let core = recording_module("core", vec![], &built);
    let app = recording_module("app", vec![core.clone()], &built);
    let unused = recording_module("unused", vec![], &built);

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(built.lock().unwrap().is_empty(), "nothing builds before it is requested");

    app.request();
    assert!(core.is_requested(), "requesting a module requests what it depends on");
    assert_eq!(app.wait().unwrap().name, "app");
    assert_eq!(core.wait().unwrap().name, "core");

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!unused.is_requested());
    assert!(!built.lock().unwrap().contains(&"unused".to_string()), "a module nobody requested never builds");

    // waiting for a module requests it, like print does
    assert_eq!(unused.wait().unwrap().name, "unused");
}

#[test]
fn a_build_holding_a_job_must_not_wait_for_unfinished_modules() {
    // waits for a module that is still building while it holds its job
    fn impatient(_: &BuildScriptContext, _: &Arguments) -> Result<Value, String> {
        let slow = PendingModule::new(ModuleCall::named("library", "slow"), vec![], |module| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            module.finish(Ok(finished_module("slow")));
        });
        slow.wait().map_err(|_| "slow failed".to_string())?;

        return Ok(Value::from(finished_module("impatient")));
    }

    let mut runtime = Runtime::new(".");
    let signature = FunctionParameters::new()
        .required("name", Type::String)
        .returns(Type::Module);
    let (name, value) = builtin("impatient", signature, impatient);
    runtime.define(name, value);

    let error = run_in(runtime, "impatient(\"impatient\")").err().unwrap();
    assert_eq!(error.to_string(), "impatient: internal error: the build panicked");
}

#[test]
fn every_module_needs_a_name_of_its_own() {
    fn library(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        return Ok(Value::from(finished_module(&args.get::<String>("name")?)));
    }

    let source = "core = library(\"core\")\napp = library(\"core\")";
    let mut runtime = Runtime::new(".").with_script(std::path::Path::new("project.build"), source);
    let signature = FunctionParameters::new()
        .required("name", Type::String)
        .returns(Type::Module);
    let (name, value) = builtin("library", signature, library);
    runtime.define(name, value);

    let script = Parser::new(source).parse_script().unwrap();
    let error = runtime.run(&script).unwrap_err();

    assert_eq!(runtime.describe(&error), "project.build:2:7: the module core is built at project.build:1:8 already, every module needs a name of its own");
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
fn commands_write_their_files_and_take_only_listed_libraries() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command-files");
    let project = system::canonical_path(&directory).unwrap();
    let file = |name: &str| project.join(name);
    system::write_file(&file("src/main.txt"), "main").unwrap();
    system::write_file(&file("taken.txt"), "a@1\n").unwrap();

    // like go.mod: the script writes the start, the command completes it next to the sources
    let script = |libraries: &str| format!(
        "build_command(\"m\", list(\"sh\", \"-c\", \"echo completed >> src/manifest; echo built >> runs.log\"), \
         files = list(file(\"src/manifest\", \"start\\n\")), \
         libraries = {libraries}, direct_libraries = list(\"cat\", \"taken.txt\"), \
         inputs = list(\"src/*.txt\"), outputs = list(\"src/manifest\"))"
    );
    let runs = || system::read_file(&file("runs.log")).unwrap_or_default().lines().count();

    build_in(&directory, &script("list(\"a@1\")"), TestSelection::None).unwrap();
    assert_eq!((runs(), system::read_file(&file("src/manifest")).unwrap()), (1, "start\ncompleted\n".to_string()));

    build_in(&directory, &script("list(\"a@1\")"), TestSelection::None).unwrap();
    assert_eq!(runs(), 1, "up to date, what the command completed is its output");

    // an editor changed what the command made of the files
    system::write_file(&file("src/manifest"), "start\nby hand\n").unwrap();
    build_in(&directory, &script("list(\"a@1\")"), TestSelection::None).unwrap();
    assert_eq!(runs(), 2, "a changed output of a command with files builds again");
    assert_eq!(system::read_file(&file("src/manifest")).unwrap(), "start\ncompleted\n");

    // a source imports another library
    system::write_file(&file("taken.txt"), "a@1\nb@2\n").unwrap();
    system::write_file(&file("src/main.txt"), "main, which takes b").unwrap();
    let error = build_in(&directory, &script("list(\"a@1\")"), TestSelection::None).unwrap_err();
    assert!(error.ends_with("m uses b@2, which its libraries do not list: build install b@2"), "{error}");

    build_in(&directory, &script("list(\"a@1\", \"b@2\")"), TestSelection::None).unwrap();
    assert_eq!(runs(), 4, "listed now, it builds");

    let outside = "build_command(\"m\", list(\"true\"), files = list(file(\"../manifest\", \"\")))";
    assert!(build_in(&directory, outside, TestSelection::None).unwrap_err().contains("files go below the directory of the command"));
    let pair = "build_command(\"m\", list(\"true\"), files = list(list(\"a\")))";
    assert!(build_in(&directory, pair, TestSelection::None).unwrap_err().contains("files takes file(path, content)"));

    // two commands that write one file would overwrite each other
    let both = "build_command(\"one\", list(\"true\"), files = list(file(\"shared\", \"1\")))\n\
                build_command(\"two\", list(\"true\"), files = list(file(\"shared\", \"2\")))";
    let error = build_in(&directory, both, TestSelection::None).unwrap_err();
    assert!(error.contains(" writes shared like "), "{error}");

    // a command whose files are written for every target
    let per_target = "build_command(\"each\", list(\"true\"), files = list(file(\"each.txt\", target())), \
                      targets = list(host_target(), \"windows-x86_64\"))";
    let errors = build_for(&directory, per_target, &[]).unwrap_err();
    assert!(errors[0].contains("the files of a command are the same for every target"), "{errors:?}");
}

/// Like a shader compiler: a command per input file, only those that changed run again
#[cfg(unix)]
#[test]
fn commands_run_once_for_every_input_and_again_for_those_that_changed() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command-per-input");
    let project = system::canonical_path(&directory).unwrap();
    let file = |name: &str| project.join(name);
    system::write_file(&file("shaders/a.vert"), "a").unwrap();
    system::write_file(&file("shaders/b.frag"), "b").unwrap();

    let script = "build_command(\"shaders\", list(\"sh\", \"-c\", \"cat $0 > $1; echo $0 >> runs.log\", \"{input}\", \"{output}/{input_name}.h\"), \
                  inputs = list(\"shaders/*.vert\", \"shaders/*.frag\"), outputs = list(\"{output}/{input_name}.h\"), per_input = true)";
    let runs = || system::read_file(&file("runs.log")).unwrap_or_default().lines().count();
    let output = |name: &str| system::read_file(&file(&format!("build/commands/shaders/{name}"))).ok();

    build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!((runs(), output("a.vert.h"), output("b.frag.h")), (2, Some("a".to_string()), Some("b".to_string())));

    build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!(runs(), 2, "nothing changed");

    // one changed, one is gone: only the changed runs, what the other left goes
    system::write_file(&file("shaders/b.frag"), "b2").unwrap();
    std::fs::remove_file(file("shaders/a.vert")).unwrap();
    build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!((runs(), output("a.vert.h"), output("b.frag.h")), (3, None, Some("b2".to_string())));

    // like glslc -MD: what a shader includes is in its depfile, a change of it builds that shader again
    system::write_file(&file("shaders/lib.glsl"), "1").unwrap();
    let with_depfile = "build_command(\"included\", list(\"sh\", \"-c\", \"cat $0 > $1; echo \\\"$1: $0 shaders/lib.glsl\\\" > $2; echo $0 >> included.log\", \
                        \"{input}\", \"{output}/{input_name}.h\", \"{output}/{input_name}.d\"), inputs = list(\"shaders/*.frag\"), \
                        outputs = list(\"{output}/{input_name}.h\"), depfile = \"{output}/{input_name}.d\", per_input = true)";
    let included = || system::read_file(&file("included.log")).unwrap_or_default().lines().count();
    build_in(&directory, with_depfile, TestSelection::None).unwrap();
    build_in(&directory, with_depfile, TestSelection::None).unwrap();
    assert_eq!(included(), 1, "nothing it includes changed");
    system::write_file(&file("shaders/lib.glsl"), "2").unwrap();
    build_in(&directory, with_depfile, TestSelection::None).unwrap();
    assert_eq!(included(), 2, "what it includes changed");

    let with_files = "build_command(\"each\", list(\"true\"), files = list(file(\"f\", \"\")), per_input = true)";
    let error = build_in(&directory, with_files, TestSelection::None).unwrap_err();
    assert!(error.contains("runs once for every input, files and direct_libraries are for a command that runs once"), "{error}");
}

/// Like the chain of shaders: a command takes what another wrote as its inputs, embed_files makes headers of it
#[cfg(unix)]
#[test]
fn commands_take_what_others_wrote_and_files_are_embedded() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command-chain");
    let project = system::canonical_path(&directory).unwrap();
    let file = |name: &str| project.join(name);
    system::write_file(&file("shaders/a.comp"), "a").unwrap();
    system::write_file(&file("data/logo.bin"), "xy").unwrap();

    let script = "compiled = build_command(\"compile\", list(\"sh\", \"-c\", \"mkdir -p build/spirv; printf abcd > build/spirv/$(basename $0).spv\", \"{input}\"), \
                  inputs = list(\"shaders/*.comp\"), outputs = list(\"build/spirv/{input_name}.spv\"), per_input = true)\n\
                  checked = build_command(\"check\", list(\"sh\", \"-c\", \"echo $0 >> checked.log\", \"{input}\"), \
                  inputs = list(\"build/spirv/*.spv\"), dependencies = list(compiled), per_input = true)\n\
                  embedded = embed_files(\"embedded\", list(compiled, \"data/*.bin\"), words = false)\n\
                  flags = prefixed(\"-D\", list(\"A=1\", \"B\"))";

    let built = build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!(system::read_file(&file("checked.log")).unwrap().lines().count(), 1, "what compile wrote is the input of check at once");

    let embedded = built.iter().find(|module| module.name == "embedded").unwrap();
    let ModuleOutput::Command(output) = &embedded.output else { panic!("embed_files makes headers like a command") };
    let names: Vec<String> = output.outputs.iter().map(|header| header.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(names, ["a.comp.spv.h", "logo.bin.h"]);
    assert!(system::read_file(&output.outputs[1]).unwrap().contains("logo_bin[] = {\n    0x78, 0x79,\n};"));

    let runtime = run(script.lines().last().unwrap()).unwrap();
    assert_eq!(runtime.scope.lock().unwrap().get("flags").map(|value| format!("{value:?}")), Some(format!("{:?}", Value::string_list(&["-DA=1", "-DB"]))));
}

/// What a command that compiles against a C library of the script needs of it, the way cgo takes it
#[cfg(unix)]
#[test]
fn commands_get_the_flags_and_the_compiler_of_their_c_libraries() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("build-command-c");
    let project = system::canonical_path(&directory).unwrap();
    let file = |name: &str| project.join(name);
    system::write_file(&file("greet/include/greet.h"), "#define GREETING \"hello\"\nconst char *greet(void);\n").unwrap();
    system::write_file(&file("greet/src/greet.c"), "#include \"greet.h\"\nconst char *greet(void) { return GREETING; }\n").unwrap();

    let script = "use c\n\
        greet = build_c(\"greet\", list(), directory = \"greet\")\n\
        generated = build_command(\"generated\", list(\"true\"))\n\
        // only the library of C\n\
        found = c_libraries(list(generated, greet))\n\
        none = if c_libraries(list(generated)) == list() { list() } else { fail(\"a command is no library of C\") }\n\
        build_command(\"cgo\", list(\"sh\", \"-c\", \"printf '%s\\\\n' \\\"$CC\\\" \\\"$CGO_CFLAGS\\\" \\\"$CGO_LDFLAGS\\\" > {output}/environment; echo run >> runs.log\"), \
            environment = list(\"CC={c_compiler}\", \"CGO_CFLAGS={compile_flags}\", \"CGO_LDFLAGS={link_flags}\"), \
            outputs = list(\"{output}/environment\"), dependencies = found + none)\n\
        // without a library of C the tool takes its own compiler\n\
        build_command(\"plain\", list(\"sh\", \"-c\", \"echo ${CC-unset} > {output}/compiler\"), \
            environment = list(\"CC={c_compiler}\"), outputs = list(\"{output}/compiler\"), dependencies = list(generated))";
    let environment = || system::read_file(&file("build/commands/cgo/environment")).unwrap();
    let runs = || system::read_file(&file("runs.log")).unwrap().lines().count();

    build_in(&directory, script, TestSelection::None).unwrap();
    let first = environment();
    let lines: Vec<&str> = first.lines().collect();
    assert_eq!(lines.len(), 3, "{first}");
    assert!(!lines[0].is_empty() && !lines[0].contains('{'), "the compiler of the library: {first}");
    assert!(lines[1].contains(&format!("-I{}", file("greet/include").display())), "{first}");
    assert!(lines[1].contains("-DBUILD_SCRIPT_HEADERS="), "{first}");
    assert!(lines[2].starts_with(&file("greet/build/lib/libgreet.a").display().to_string()), "{first}");
    assert_eq!(system::read_file(&file("build/commands/plain/compiler")).unwrap().trim(), std::env::var("CC").unwrap_or("unset".to_string()));

    build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!(runs(), 1, "up to date");

    // only a header changed: the command runs again, and a cache keyed by the flags, like that of Go, misses
    system::write_file(&file("greet/include/greet.h"), "#define GREETING \"hi\"\nconst char *greet(void);\n").unwrap();
    build_in(&directory, script, TestSelection::None).unwrap();
    assert_eq!(runs(), 2, "a header of the library builds the command again");
    assert_ne!(environment().lines().nth(1), first.lines().nth(1), "the define names the headers");
}

/// The modules of a build of `source` for the machine and the other targets, like the command line does it,
/// on one job: a build that waits for another while it holds the job would never end
#[cfg(unix)]
fn build_for(directory: &std::path::Path, source: &str, targets: &[Target]) -> Result<Vec<Arc<BuiltModule>>, Vec<String>> {
    let script = Parser::new(source)
        .parse_script()
        .expect("script should parse");
    let wanted = configurations::Wanted { targets, runs: false };
    let evaluations = configurations::evaluate(&script, wanted, |target| {
        Runtime::new(directory)
            .with_script(std::path::Path::new("project.build"), source)
            .with_jobs(1)
            .with_target(target)
    });

    return evaluations.finish();
}

#[cfg(unix)]
#[test]
fn scripts_name_the_targets_of_their_modules() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("targets");
    let project = system::canonical_path(&directory).unwrap();
    // never this machine, the tests with sh run on others
    let windows = Target::named("windows-x86_64").unwrap();
    let file = |path: &str| project.join(path);

    // build/<target>/ for another target, nothing for the machine when the script leaves it out
    let info = |targets: &str| format!(
        "build_command(\"info\", list(\"sh\", \"-c\", \"echo \" + target() + \" \" + executable(\"app\") + \" > {{output}}/info\"), \
         outputs = list(\"{{output}}/info\"), targets = {targets})"
    );
    build_for(&directory, &info("list(\"windows-x86_64\")"), &[]).unwrap();
    assert_eq!(system::read_file(&file("build/windows-x86_64/commands/info/info")).unwrap().trim(), "windows-x86_64 app.exe");
    assert!(!file("build/commands/info").exists(), "the machine is no target of it");

    let built = build_for(&directory, &info("list(host_target(), \"windows-x86_64\")"), &[]).unwrap();
    assert!(file("build/commands/info/info").exists(), "host_target() names the machine");
    assert_eq!(built.len(), 2);

    // build --target instead of what the script names
    system::remove_dir(&file("build")).unwrap();
    build_for(&directory, &info("list(\"linux-aarch64\")"), &[windows]).unwrap();
    assert!(file("build/windows-x86_64/commands/info/info").exists());
    assert!(!file("build/linux-aarch64").exists(), "--target replaces the targets of the script");

    // a module without targets follows those that need it, the machine builds it when nothing else does: one
    // that differs by target, an unchanged command would be the machine's
    system::remove_dir(&file("build")).unwrap();
    let part = "part = build_command(\"part\", list(\"sh\", \"-c\", \"echo \" + target() + \" > {output}/part\"), outputs = list(\"{output}/part\"))\n";
    let whole = |targets: &str| format!(
        "{part}build_command(\"whole\", list(\"sh\", \"-c\", \"echo whole > {{output}}/whole\"), outputs = list(\"{{output}}/whole\"), \
         dependencies = list(part), targets = {targets})"
    );
    build_for(&directory, &whole("list(\"windows-x86_64\")"), &[]).unwrap();
    assert!(file("build/windows-x86_64/commands/part/part").exists(), "it follows the module that needs it");
    assert!(!file("build/commands/part").exists(), "only a module for another target needs it");

    build_for(&directory, part, &[]).unwrap();
    assert!(file("build/commands/part/part").exists(), "no module needs it, it is built for the machine");
}

#[cfg(unix)]
#[test]
fn builds_for_other_targets_run_the_programs_of_the_machine_once() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("targets-programs");
    let project = system::canonical_path(&directory).unwrap();
    let file = |path: &str| project.join(path);

    // a program that says what it was made for, run by a command for windows-x86_64 only, which also needs a
    // generated file that is the same for every target
    let source = "\
        tool = build_command(\"tool\", list(\"sh\", \"-c\", \"echo \" + target() + \" > {output}/made-for\"), \
            outputs = list(\"{output}/made-for\"), run = list(\"cp\", \"{output}/made-for\"))\n\
        same = build_command(\"same\", list(\"sh\", \"-c\", \"echo run >> runs; echo > {output}/same\"), outputs = list(\"{output}/same\"))\n\
        got = build_command(\"got\", list(tool, \"{output}/got\"), outputs = list(\"{output}/got\"), dependencies = list(same), \
            targets = list(\"windows-x86_64\"))";

    build_for(&directory, source, &[]).unwrap();
    assert_eq!(system::read_file(&file("build/windows-x86_64/commands/got/got")).unwrap().trim(), Target::host().name(), "the program of the machine");
    assert!(!file("build/windows-x86_64/commands/tool").exists(), "the program is not built for the other target");
    assert!(!file("build/commands/got").exists(), "got names its targets, the machine is none of them");

    // a command without targets that is the same for every target runs once, that of the machine
    assert!(file("build/commands/same/same").exists());
    assert!(!file("build/windows-x86_64/commands/same").exists());
    assert_eq!(system::read_file(&file("runs")).unwrap().lines().count(), 1);
}

#[test]
fn modules_built_once_for_every_target_come_from_the_machine() {
    fn library(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        return Ok(Value::from(finished_module(&args.get::<String>("name")?)));
    }

    let windows = Target::named("windows-x86_64").unwrap();
    let other = if Target::host() == windows { Target::named("linux-aarch64").unwrap() } else { windows };
    let with_library = |target: Option<Target>| {
        let mut runtime = Runtime::new(".").with_script(std::path::Path::new("project.build"), "").with_target(target);
        let signature = FunctionParameters::new()
            .required("name", Type::String)
            .returns(Type::Module)
            .placed(Placement::HostOnly);
        let (name, value) = builtin("library", signature, library);
        runtime.define(name, value);
        return runtime;
    };

    let machine = run_in(with_library(None), "core = library(\"core\")").unwrap();
    let theirs = run_in(with_library(Some(other)).after_machine(&machine), "core = library(\"core\")").unwrap();
    let (Value::Module(mine), Value::Module(theirs)) = (variable(&machine, "core"), variable(&theirs, "core")) else {
        panic!("core is no module");
    };
    assert!(Arc::ptr_eq(&mine, &theirs), "one module for every target");

    // a module only the other target calls has no machine's module to be
    let source = "only = if target() == host_target() { \"\" } else { library(\"only\") }";
    let machine = run_in(with_library(None), source).unwrap();
    let error = run_in(with_library(Some(other)).after_machine(&machine), source).err().unwrap();
    assert!(error.to_string().contains("but the script builds it only for"), "{error}");
}

#[test]
fn a_placeholder_stands_for_a_module_of_a_later_evaluation() {
    let built = Arc::new(Mutex::new(vec![]));
    let library = recording_module("greeter", vec![], &built);
    let placeholder = PendingModule::placeholder(ModuleCall::named("build_android", "greeter"));

    // requested before it knows its module, it requests that one once it does
    placeholder.request();
    assert!(!placeholder.stands_for_a_module());
    placeholder.stand_for(&library);
    assert!(library.is_requested());
    assert_eq!(placeholder.wait().unwrap().name, "greeter");

    // one that learns its module after that is done is done at once
    let late = PendingModule::placeholder(ModuleCall::named("build_android", "greeter"));
    late.stand_for(&library);
    assert_eq!(late.wait().unwrap().name, "greeter");
}

#[test]
fn apps_pack_the_libraries_their_android_targets_build() {
    static PACKED: Mutex<Vec<String>> = Mutex::new(vec![]);
    static BUILT: Mutex<Vec<String>> = Mutex::new(vec![]);

    // a library of C for every target, its artifact names the target
    fn build_c(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let name: String = args.get("name")?;
        BUILT.lock().unwrap().push(format!("{name} {}", context.target().name()));
        let mut module = Arc::try_unwrap(finished_module(&name)).unwrap();
        module.artifact = PathBuf::from(format!("{}/lib{name}.so", context.target().name()));
        return Ok(Value::from(Arc::new(module)));
    }

    // an apk: what it packs of the evaluations for its ABIs
    fn app(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let mut packed: Vec<String> = args.packed_libraries()
            .iter()
            .map(|(target, module)| format!("{} {}", target.name(), module.artifact.display()))
            .collect();
        packed.sort();
        *PACKED.lock().unwrap() = packed;
        return Ok(Value::from(finished_module(&args.get::<String>("name")?)));
    }

    let runtime_for = |target: Option<Target>| {
        let mut runtime = Runtime::new(".").with_script(std::path::Path::new("project.build"), "").with_target(target).with_jobs(1);
        let library = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
            .returns(Type::Module);
        let apk = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("dependencies", Type::list(Type::Module), Value::List(vec![]))
            .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
            .with_default("min_sdk", Type::Int, Value::Int(24))
            .with_default("ndk_version", Type::String, "28")
            .with_default("accept_sdk_license", Type::Bool, false)
            .returns(Type::Module)
            .placed(Placement::HostOnly)
            .packing(Packing::ForItsAbis);
        for (name, value) in [builtin("build_c", library, build_c), builtin("app", apk, app)] {
            runtime.define(name, value);
        }
        return runtime;
    };

    let build = |source: &str| {
        let script = Parser::new(source).parse_script().unwrap();
        let wanted = configurations::Wanted { targets: &[], runs: false };
        return configurations::evaluate(&script, wanted, runtime_for).finish();
    };

    build("greeter = build_c(\"greeter\")\napp(\"app\", list(greeter), targets = list(\"android-aarch64\", \"android-x86_64\"))").unwrap();
    assert_eq!(*PACKED.lock().unwrap(), [
        "android-aarch64 android-aarch64/libgreeter.so",
        "android-x86_64 android-x86_64/libgreeter.so",
    ]);

    // one that names its targets is packed for those of the app's, and without the machine among them it is not
    // built for the machine
    BUILT.lock().unwrap().clear();
    build("greeter = build_c(\"greeter\", targets = list(\"android-aarch64\"))\napp(\"app\", list(greeter), targets = list(\"android-aarch64\", \"android-x86_64\"))").unwrap();
    assert_eq!(*PACKED.lock().unwrap(), ["android-aarch64 android-aarch64/libgreeter.so"]);
    assert_eq!(*BUILT.lock().unwrap(), ["greeter android-aarch64"]);

    let errors = build("greeter = build_c(\"greeter\", targets = list(\"linux-x86_64\"))\napp(\"app\", list(greeter), targets = list(\"android-aarch64\"))").unwrap_err();
    assert!(errors[0].contains("greeter is built for linux-x86_64, none of them a target of this app: add one of android-aarch64 to its targets"), "{errors:?}");

    // a library the evaluation of a target does not build has no module to pack
    let errors = build(
        "greeter = build_c(\"greeter\")\nother = if target() == \"android-x86\" { greeter } else { build_c(\"other\") }\napp(\"app\", list(other), targets = list(\"android-x86\"))",
    ).unwrap_err();
    assert!(errors.iter().any(|error| error.contains("the script builds no module other for android-x86")), "{errors:?}");

    // an app is for Android
    let errors = build("greeter = build_c(\"greeter\")\napp(\"app\", list(greeter), targets = list(\"linux-aarch64\"))").unwrap_err();
    assert!(errors[0].contains("an Android app is built for Android targets only"), "{errors:?}");
}

#[test]
fn jars_pack_the_libraries_for_the_targets_they_name() {
    static PACKED: Mutex<Vec<String>> = Mutex::new(vec![]);
    static BUILT: Mutex<Vec<String>> = Mutex::new(vec![]);

    fn build_c(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let name: String = args.get("name")?;
        BUILT.lock().unwrap().push(format!("{name} {}", context.target().name()));
        let mut module = Arc::try_unwrap(finished_module(&name)).unwrap();
        module.artifact = PathBuf::from(format!("{}/lib{name}.so", context.target().name()));
        return Ok(Value::from(Arc::new(module)));
    }

    fn jar(_: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let mut packed: Vec<String> = args.packed_libraries()
            .iter()
            .map(|(target, module)| format!("{} {}", target.name(), module.artifact.display()))
            .collect();
        packed.sort();
        *PACKED.lock().unwrap() = packed;
        return Ok(Value::from(finished_module(&args.get::<String>("name")?)));
    }

    let runtime_for = |target: Option<Target>| {
        let mut runtime = Runtime::new(".").with_script(std::path::Path::new("project.build"), "").with_target(target).with_jobs(1);
        let library = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
            .returns(Type::Module);
        let jar_parameters = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("dependencies", Type::list(Type::Module), Value::List(vec![]))
            .returns(Type::Module)
            .placed(Placement::HostOnly)
            .packing(Packing::ForTheirTargets);
        for (name, value) in [builtin("build_c", library, build_c), builtin("build_kotlin", jar_parameters, jar)] {
            runtime.define(name, value);
        }
        return runtime;
    };

    let build = |source: &str| {
        BUILT.lock().unwrap().clear();
        let script = Parser::new(source).parse_script().unwrap();
        let wanted = configurations::Wanted { targets: &[], runs: false };
        return configurations::evaluate(&script, wanted, runtime_for).finish();
    };

    // the machine's own module for the machine, those of the other evaluations for the others, none for Android:
    // its apps pack by ABI
    let host = Target::host().name();
    build(&format!(
        "greeter = build_c(\"greeter\", targets = list(host_target(), \"linux-aarch64\", \"windows-x86_64\", \"android-x86\", host_target()))\n\
         plain = build_c(\"plain\")\n\
         build_kotlin(\"app\", list(greeter, plain))"
    )).unwrap();
    let mut expected = vec![
        format!("{host} {host}/libgreeter.so"),
        "linux-aarch64 linux-aarch64/libgreeter.so".to_string(),
        "windows-x86_64 windows-x86_64/libgreeter.so".to_string(),
    ];
    expected.sort();
    expected.dedup();
    assert_eq!(*PACKED.lock().unwrap(), expected, "a library without targets stays outside");

    // the library is built for the targets it names, plain for the machine, which runs the jar
    let mut built = BUILT.lock().unwrap().clone();
    built.sort();
    let mut expected = vec![
        format!("greeter {host}"),
        "greeter android-x86".to_string(),
        "greeter linux-aarch64".to_string(),
        "greeter windows-x86_64".to_string(),
        format!("plain {host}"),
    ];
    expected.sort();
    expected.dedup();
    assert_eq!(built, expected);

    // run and test get the modules of the machine, the jar still packs those of the other targets
    let source = "greeter = build_c(\"greeter\", targets = list(host_target(), \"linux-aarch64\"))\nbuild_kotlin(\"app\", list(greeter))";
    let script = Parser::new(source).parse_script().unwrap();
    let wanted = configurations::Wanted { targets: &[], runs: true };
    let modules = configurations::evaluate(&script, wanted, runtime_for).finish().unwrap();
    let mut names: Vec<String> = modules.iter().map(|module| format!("{} {}", module.name, module.artifact.display())).collect();
    names.sort();
    assert_eq!(names, ["app app.jar".to_string(), format!("greeter {host}/libgreeter.so")]);
    assert!(PACKED.lock().unwrap().contains(&"linux-aarch64 linux-aarch64/libgreeter.so".to_string()));
}

#[test]
fn run_and_test_get_the_modules_of_their_target() {
    static BUILT: Mutex<Vec<String>> = Mutex::new(vec![]);

    fn build_c(context: &BuildScriptContext, args: &Arguments) -> Result<Value, String> {
        let name: String = args.get("name")?;
        BUILT.lock().unwrap().push(format!("{name} {}", context.target().name()));
        let mut module = Arc::try_unwrap(finished_module(&name)).unwrap();
        module.artifact = PathBuf::from(format!("{}/{name}", context.target().name()));
        return Ok(Value::from(Arc::new(module)));
    }

    let runtime_for = |target: Option<Target>| {
        let mut runtime = Runtime::new(".").with_script(std::path::Path::new("project.build"), "").with_target(target).with_jobs(1);
        let parameters = FunctionParameters::new()
            .required("name", Type::String)
            .with_default("targets", Type::list(Type::target()), Value::List(vec![]))
            .returns(Type::Module);
        let (name, value) = builtin("build_c", parameters, build_c);
        runtime.define(name, value);
        return runtime;
    };

    // what run and test get, what was built
    let run = |targets: &[Target]| {
        BUILT.lock().unwrap().clear();
        let script = Parser::new("build_c(\"tool\", targets = list(\"windows-x86_64\"))\nbuild_c(\"other\")").parse_script().unwrap();
        let wanted = configurations::Wanted { targets, runs: true };
        let modules = configurations::evaluate(&script, wanted, runtime_for).finish().unwrap();
        let mut got: Vec<String> = modules.iter().map(|module| module.artifact.display().to_string()).collect();
        let mut built = BUILT.lock().unwrap().clone();
        got.sort();
        built.sort();
        return (got, built);
    };

    let host = Target::host().name();
    let linux = Target::named("linux-aarch64").unwrap();

    // without --target the machine's, with one that target's, also of modules that name others
    let mut expected = vec![format!("{host}/other"), format!("{host}/tool")];
    expected.sort();
    assert_eq!(run(&[]).0, expected, "run builds every module for the machine");
    assert_eq!(run(&[linux]), (
        vec!["linux-aarch64/other".to_string(), "linux-aarch64/tool".to_string()],
        vec!["other linux-aarch64".to_string(), "tool linux-aarch64".to_string()],
    ));
    assert_eq!(run(&[Target::host()]).0, expected);
}

#[cfg(unix)]
#[test]
fn errors_of_other_targets_say_which() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("targets-errors");
    let source = "build_command(\"broken\", list(\"sh\", \"-c\", \"exit 3\"), targets = list(\"windows-x86_64\"))";
    let errors = build_for(&directory, source, &[]).unwrap_err();

    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("project.build:1:1 [windows-x86_64]: build_command:"), "{}", errors[0]);
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
    assert_eq!(launch(&built[0], &["7".to_string()], &Runner::direct()), Ok(7), "run takes the arguments after --");

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

#[test]
fn lists_join_and_text_replaces_without_loops() {
    let runtime = run(r#"
        libraries = list("github.com/spf13/cobra@v1.9.1", "golang.org/x/text@v0.24.0")
        lines = replace(join(libraries, "\n\t"), "@", " ")
        none = join(list(), ",")
        one = join(list("a"), ", ")
        go_mod = file("src/go.mod", "module x\n")
        empty = libraries == list()
    "#)
    .unwrap();

    assert_eq!(string(&runtime, "lines"), "github.com/spf13/cobra v1.9.1\n\tgolang.org/x/text v0.24.0");
    assert_eq!(string(&runtime, "none"), "");
    assert_eq!(string(&runtime, "one"), "a");
    let Value::List(file) = variable(&runtime, "go_mod") else { panic!("file gives a list") };
    assert_eq!(file.iter().map(Value::to_string).collect::<Vec<_>>(), ["src/go.mod", "module x\n"]);
    assert_eq!(string(&runtime, "empty"), "false");

    let message = |source: &str| run(source).err().expect(source).to_string();
    assert_eq!(message("replace(\"abc\", \"\", \"x\")"), "replace: replace needs a text to replace, old is empty");
    assert!(message("join(list(1, 2), \",\")").contains("list<str>"), "{}", message("join(list(1, 2), \",\")"));
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

    let (_, result) = plugin_runtime(&directory, "use \"someone\"");
    assert!(result.unwrap_err().contains("someone names no repository"));
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

/// A bundle is what a library is in every language: bundled() hands its parts to the functions of plugins, a C
/// module takes its modules, a command runs with what those need
#[cfg(unix)]
#[test]
fn bundles_give_each_language_its_part_and_their_modules_to_all() {
    use crate::common::test_support::ScratchDir;

    let directory = ScratchDir::new("bundle");
    let project = system::canonical_path(&directory).unwrap();
    let script = "use c\n\
        runtime = prebuilt_library(\"vk-runtime\", environment = list(\"VK_LAYER=on\"), jvm_flags = list(\"-Dvk=1\"))\n\
        inner = bundle(\"inner\", ports = list(\"vulkan-headers\"), go = list(\"example.com/vk@v1\"))\n\
        vk = bundle(\"vk\", ports = list(\"vulkan-loader\", \"vulkan-headers\"), maven = list(\"org.lwjgl:lwjgl:3.4.3\"), \
            go = list(\"example.com/vk@v1\", \"example.com/glfw@v2\"), modules = list(runtime, inner))\n\
        generated = build_command(\"generated\", list(\"true\"))\n\
        ports = bundled(list(\"glfw3\", vk, generated), \"ports\")\n\
        maven = bundled(list(\"glfw3\", vk), \"maven\")\n\
        go = bundled(list(vk), \"go\")\n\
        modules = bundled_modules(list(vk, generated))\n\
        headers = build_c(\"headers\", list(vk))\n\
        program = build_command(\"program\", list(\"true\"), dependencies = modules, run = list(\"true\"))";

    let runtime = run_in(Runtime::new(&project), script).unwrap();
    let strings = |name: &str| format!("{:?}", variable(&runtime, name));
    assert_eq!(strings("ports"), format!("{:?}", Value::string_list(&["glfw3", "vulkan-loader", "vulkan-headers"])));
    assert_eq!(strings("maven"), format!("{:?}", Value::string_list(&["org.lwjgl:lwjgl:3.4.3"])));
    assert_eq!(strings("go"), format!("{:?}", Value::string_list(&["example.com/vk@v1", "example.com/glfw@v2"])));

    let Value::List(modules) = variable(&runtime, "modules") else { panic!("bundled_modules gives a list") };
    let names: Vec<String> = modules.iter()
        .map(|module| match module {
            Value::Module(module) => module.wait().unwrap().name.clone(),
            other => panic!("{other:?} is no module"),
        })
        .collect();
    assert_eq!(names, ["vk-runtime", "generated"], "the modules of the bundle instead of it");

    let built = |name: &str| runtime.module_named(name).unwrap().wait().unwrap();
    let ModuleOutput::Native(headers) = &built("headers").output else { panic!("build_c makes a C module") };
    assert_eq!(headers.environment, [("VK_LAYER".to_string(), "on".to_string())], "a C module takes the modules of a bundle");

    let ModuleOutput::Command(program) = &built("program").output else { panic!("a command") };
    assert_eq!(program.runtime.environment, [("VK_LAYER".to_string(), "on".to_string())], "what the program runs with");
}
