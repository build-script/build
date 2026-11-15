// The modules builds produce: built, still building or failed, what they put out and how `run` starts them
use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::command::{self, CommandOutput};
use crate::jvm::{self, JvmOutput};
use crate::native::NativeOutput;
use crate::language::lexer::Span;

use super::*;

/// What building a module produced. Other modules can depend on it, like
/// `implementation(project(":core"))` in Gradle.
#[derive(Debug)]
pub struct BuiltModule {
    pub name: String,
    /// where the module lives, programs run there
    pub directory: PathBuf,
    /// the file to run or install, what the module is in a script
    pub artifact: PathBuf,
    /// modules of the script it depends on
    pub modules: Vec<Arc<BuiltModule>>,
    pub output: ModuleOutput,
    /// empty when `build test` did not select it
    pub tests: Vec<TestSuite>,
}

/// A module that may still be building on its own thread, whatever needs it waits for it. It starts once
/// something requests it: the script, `run`, `test` or a module that depends on it.
pub struct PendingModule {
    /// what builds it and where the script calls it, for its error
    pub function: &'static str,
    pub span: Span,
    /// built by a function of a plugin: that function and where the script called it
    pub called_from: Option<(String, Span)>,
    /// the name its build function was given, empty for modules a build function returned
    pub name: String,
    /// the modules among its arguments, requested together with it
    dependencies: Vec<Arc<PendingModule>>,
    requested: AtomicBool,
    /// the build on a thread of its own, taken when the module is requested
    start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    result: Mutex<Option<Result<Arc<BuiltModule>, BuildFailure>>>,
    done: Condvar,
}

impl std::fmt::Debug for PendingModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f.debug_struct("PendingModule")
            .field("function", &self.function)
            .field("name", &self.name)
            .field("requested", &self.is_requested())
            .finish_non_exhaustive();
    }
}

thread_local! {
    /// set while a build holds one of the job slots: waiting for an unfinished module then could take all of
    /// them while the modules that would finish wait for a slot
    static HOLDS_JOB: Cell<bool> = const { Cell::new(false) };
}

/// Marks the thread as holding a job slot until it is dropped, also when the build panics
pub struct JobHeld;

impl JobHeld {
    pub fn new() -> Self {
        HOLDS_JOB.with(|holds| holds.set(true));
        return JobHeld;
    }
}

impl Drop for JobHeld {
    fn drop(&mut self) {
        HOLDS_JOB.with(|holds| holds.set(false));
    }
}

#[derive(Debug, Clone)]
pub enum BuildFailure {
    /// what the build function said
    Failed(String),
    /// a module it depends on failed, that one says why
    DependencyFailed,
}

/// What a language category keeps of a module for the modules depending on it and for `build run`
#[derive(Debug)]
pub enum ModuleOutput {
    Jvm(JvmOutput),
    Native(NativeOutput),
    /// of build_command
    Command(CommandOutput),
}

/// Starts what a module built and returns the exit code for the process
pub fn launch(module: &BuiltModule, args: &[String]) -> Result<i32, String> {
    match &module.output {
        ModuleOutput::Jvm(output) => return jvm::launch(module, output, args),
        ModuleOutput::Native(output) => return crate::native::launch(module, output, args),
        ModuleOutput::Command(output) => return command::launch(module, output, args),
    }
}

impl PendingModule {
    /// A module of the script, not started yet: `build` starts on a thread of its own once it is requested
    pub fn new(
        function: &'static str,
        span: Span,
        called_from: Option<(String, Span)>,
        name: String,
        dependencies: Vec<Arc<PendingModule>>,
        build: impl FnOnce(&PendingModule) + Send + 'static,
    ) -> Arc<Self> {
        return Arc::new_cyclic(|this: &std::sync::Weak<PendingModule>| {
            // weak: a module never requested would keep itself alive through its own build
            let this = this.clone();
            let start: Box<dyn FnOnce() + Send> = Box::new(move || {
                if let Some(module) = this.upgrade() {
                    build(&module);
                }
            });

            return PendingModule {
                function,
                span,
                called_from,
                name,
                dependencies,
                requested: AtomicBool::new(false),
                start: Mutex::new(Some(start)),
                result: Mutex::new(None),
                done: Condvar::new(),
            };
        });
    }

    /// Of a build function that built it right away
    pub fn built(module: Arc<BuiltModule>) -> Arc<Self> {
        let pending = Arc::new(PendingModule {
            function: "",
            span: Span::new(0, 0),
            called_from: None,
            name: String::new(),
            dependencies: vec![],
            requested: AtomicBool::new(true),
            start: Mutex::new(None),
            result: Mutex::new(None),
            done: Condvar::new(),
        });
        pending.finish(Ok(module));

        return pending;
    }

    pub fn finish(&self, result: Result<Arc<BuiltModule>, BuildFailure>) {
        *self.result.lock().unwrap() = Some(result);
        self.done.notify_all();
    }

    /// Starts its build and those of the modules it depends on, once
    pub fn request(&self) {
        if self.requested.swap(true, Ordering::SeqCst) {
            return;
        }

        for dependency in &self.dependencies {
            dependency.request();
        }

        let start = self.start
            .lock().unwrap()
            .take();

        if let Some(start) = start {
            std::thread::spawn(start);
        }
    }

    pub fn is_requested(&self) -> bool {
        return self.requested.load(Ordering::SeqCst);
    }

    /// What the build gave, once it is done. A module not requested yet is requested first.
    pub fn wait(&self) -> Result<Arc<BuiltModule>, BuildFailure> {
        self.request();
        let mut result = self.result.lock().unwrap();

        // a check in debug builds and tests, also those of CI with --release: a build waits for the modules it
        // depends on before it takes its slot
        if cfg!(any(debug_assertions, test)) && result.is_none() && HOLDS_JOB.with(Cell::get) {
            panic!("a build holding a job waits for {} {:?}", self.function, self.name);
        }

        while result.is_none() {
            result = self.done.wait(result).unwrap();
        }

        return result.clone().unwrap();
    }
}

impl BuiltModule {
    /// The given modules and everything they depend on, each once, dependencies first
    pub fn transitive(modules: &[Arc<BuiltModule>]) -> Vec<Arc<BuiltModule>> {
        // a module reached again is not descended into again: modules many
        // others depend on would be visited once per path to them
        fn visit(
            module: &Arc<BuiltModule>,
            seen: &mut HashSet<*const BuiltModule>,
            out: &mut Vec<Arc<BuiltModule>>,
        ) {
            if !seen.insert(Arc::as_ptr(module)) {
                return;
            }

            for dependency in &module.modules {
                visit(dependency, seen, out);
            }

            out.push(module.clone());
        }

        let mut seen = HashSet::new();
        let mut out = vec![];

        for module in modules {
            visit(module, &mut seen, &mut out);
        }

        return out;
    }

    /// Whether `build run` can start it
    pub fn can_launch(&self) -> bool {
        match &self.output {
            ModuleOutput::Jvm(output) => return output.launch.is_some(),
            ModuleOutput::Native(output) => return output.program.is_some(),
            ModuleOutput::Command(output) => return !output.run.is_empty(),
        }
    }
}

impl From<Arc<BuiltModule>> for Value {
    fn from(value: Arc<BuiltModule>) -> Self {
        return Value::Module(PendingModule::built(value));
    }
}
