// What every language category uses: the host system, up-to-date checks, XML, sources from git, questions
// in the terminal
pub mod git;
pub mod lock;
pub mod refresh;
pub mod semaphore;
pub mod settings;
pub mod system;
pub mod terminal;
#[cfg(test)]
pub mod test_support;
pub mod up_to_date;
pub mod xml;
