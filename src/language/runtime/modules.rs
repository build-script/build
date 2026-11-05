// The modules builds produce: built, still building or failed, what they put out and how `run` starts them
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

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

/// A module that may still be building on its own thread, whatever needs it waits for it
#[derive(Debug)]
pub struct PendingModule {
    /// what builds it and where the script calls it, for its error
    pub function: &'static str,
    pub span: Span,
    result: Mutex<Option<Result<Arc<BuiltModule>, BuildFailure>>>,
    done: Condvar,
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
}

/// Starts what a module built and returns the exit code for the process
pub fn launch(module: &BuiltModule, args: &[String]) -> Result<i32, String> {
    match &module.output {
        ModuleOutput::Jvm(output) => return jvm::launch(module, output, args),
        ModuleOutput::Native(output) => return crate::native::launch(module, output, args),
    }
}

impl PendingModule {
    pub fn new(function: &'static str, span: Span) -> Arc<Self> {
        return Arc::new(PendingModule {
            function,
            span,
            result: Mutex::new(None),
            done: Condvar::new(),
        });
    }

    /// Of a build function that built it right away
    pub fn built(module: Arc<BuiltModule>) -> Arc<Self> {
        let pending = Self::new("", Span::new(0, 0));
        pending.finish(Ok(module));

        return pending;
    }

    pub fn finish(&self, result: Result<Arc<BuiltModule>, BuildFailure>) {
        *self.result.lock().unwrap() = Some(result);
        self.done.notify_all();
    }

    /// What the build gave, once it is done
    pub fn wait(&self) -> Result<Arc<BuiltModule>, BuildFailure> {
        let mut result = self.result.lock().unwrap();

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
        }
    }
}

impl From<Arc<BuiltModule>> for Value {
    fn from(value: Arc<BuiltModule>) -> Self {
        return Value::Module(PendingModule::built(value));
    }
}
