// The evaluations of a script a build needs: one for the machine, then one for every other target the
// script names or build --target asks for. They run one after another, the builds they start side by side.
use std::sync::Arc;

use crate::common::target::Target;
use crate::common::terminal;
use crate::language::parser::Script;

use super::{BuiltModule, Requests, Runtime};

/// The runtimes of a build, the machine's first
pub struct Evaluations {
    pub machine: Runtime,
    pub others: Vec<Runtime>,
    /// the first error of an evaluation, described by its runtime: no evaluation follows it
    pub error: Option<String>,
    /// run and test: the targets whose modules they run, the others are built for what packs them
    ran: Option<Vec<Target>>,
}

/// What a build is for
pub struct Wanted<'a> {
    /// build --target: those instead of the targets the script names, run and test --target: the one they run
    pub targets: &'a [Target],
    /// run and test: every module for the target they run, the machine without --target, and of the other
    /// targets only what modules for that one pack
    pub runs: bool,
}

/// Evaluates the script for the machine, then for the other targets. `runtime_for` makes the runtime of a
/// target with what the command line gives, none is the machine.
pub fn evaluate(script: &Script, wanted: Wanted, runtime_for: impl Fn(Option<Target>) -> Runtime) -> Evaluations {
    let Wanted { targets, runs } = wanted;

    // the machine first: the modules built once for every target and the programs builds run come from it
    let machine_requests = match (runs, targets.is_empty()) {
        (true, true) => Requests::All,
        (false, true) => Requests::Declared,
        (_, false) if targets.iter().any(Target::is_host) => Requests::All,
        (_, false) => Requests::HostOnly,
    };
    let mut machine = runtime_for(None).with_requests(machine_requests);
    let mut error = machine.run(script).err().map(|error| machine.describe(&error));

    let mut others: Vec<(Target, Requests)> = match (runs, targets.is_empty()) {
        (true, true) => vec![],
        (false, true) => machine.declared_targets()
            .into_iter()
            .map(|target| (target, Requests::Declared))
            .collect(),
        (_, false) => targets.iter()
            .filter(|target| !target.is_host())
            .map(|target| (*target, Requests::All))
            .collect(),
    };

    // the targets the machine's modules need modules of, like the ABIs of apks: those build only what they need
    for target in machine.forward_targets() {
        if !others.iter().any(|(other, _)| *other == target) {
            others.push((target, Requests::Declared));
        }
    }
    let mut evaluations = vec![];

    for (target, requests) in others {
        if error.is_some() {
            break;
        }

        let mut runtime = runtime_for(Some(target))
            .with_requests(requests)
            .after_machine(&machine);
        terminal::set_target_label(Some(target.name()));
        error = runtime.run(script).err().map(|error| runtime.describe(&error));
        terminal::set_target_label(None);
        evaluations.push(runtime);
    }

    // a placeholder no evaluation named a module for would be waited for forever
    let without_module = machine.fail_forwards_without_module();
    let error = error.or_else(|| (!without_module.is_empty()).then(|| without_module.join("\n")));

    // run and test get the modules of the target they run
    let ran = match (runs, targets.is_empty()) {
        (false, _) => None,
        (true, true) => Some(vec![Target::host()]),
        (true, false) => Some(targets.to_vec()),
    };

    return Evaluations { machine, others: evaluations, error, ran };
}

impl Evaluations {
    /// Waits for the builds still running: the modules of every target, the machine's first, for run and test
    /// only those of the target they run, or why builds failed. Builds that started before an error finish,
    /// none of their tools keep running.
    pub fn finish(&self) -> Result<Vec<Arc<BuiltModule>>, Vec<String>> {
        let mut modules = vec![];
        let mut errors: Vec<String> = self.error.iter().cloned().collect();

        for runtime in std::iter::once(&self.machine).chain(&self.others) {
            let target = runtime.context.target.unwrap_or_else(Target::host);
            let ran = self.ran.as_ref().is_none_or(|ran| ran.contains(&target));

            match runtime.finish() {
                Ok(built) if ran => modules.extend(built),
                Ok(_) => {}
                Err(failed) => errors.extend(failed.iter().map(|error| runtime.describe(error))),
            }
        }

        match errors.is_empty() {
            true => return Ok(modules),
            false => return Err(errors),
        }
    }
}
