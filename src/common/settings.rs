// settings: what the dev chose once, e.g. domain for new projects
use std::path::{Path, PathBuf};

use crate::common::system;

pub struct Settings {
    file: PathBuf,
    /// in order
    entries: Vec<(String, String)>,
}

impl Settings {
    /// The file `settings` in the config directory
    pub fn load() -> Result<Self, String> {
        return Self::load_from(system::config_dir().join("settings"));
    }

    pub fn load_from(file: PathBuf) -> Result<Self, String> {
        let content = if file.is_file() {
            system::read_file(&file)?
        } else {
            String::new()
        };

        let mut entries = vec![];

        for (number, line) in content.lines().enumerate() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((name, value)) = line.split_once('=') else {
                return Err(format!(
                    "{}:{}: expected name = value",
                    file.display(),
                    number + 1
                ));
            };

            entries.push((name.trim().to_string(), value.trim().to_string()));
        }

        return Ok(Settings { file, entries });
    }

    pub fn file(&self) -> &Path {
        return &self.file;
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        return self.entries.iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str());
    }

    /// Saves the file
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let kv_pair = self.entries.iter_mut().find(|(known, _)| known == name);

        match kv_pair {
            Some(entry) => entry.1 = value.to_string(),
            None => self.entries.push((name.to_string(), value.to_string())),
        }

        return self.save();
    }

    /// False if it was not set
    pub fn remove(&mut self, name: &str) -> Result<bool, String> {
        let before = self.entries.len();

        self.entries.retain(|(known, _)| known != name);
        if self.entries.len() == before {
            return Ok(false);
        }

        self.save()?;

        return Ok(true);
    }

    fn save(&self) -> Result<(), String> {
        let lines: Vec<String> = self.entries.iter()
            .map(|(name, value)| format!("{name} = {value}\n"))
            .collect();

        return system::write_file(
            &self.file,
            &format!("# written by build config\n{}", lines.concat()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_read_changed_and_written() {
        let file =
            std::env::temp_dir().join(format!("build-script-settings-{}", std::process::id()));

        let _ = std::fs::remove_file(&file);

        let mut settings = Settings::load_from(file.clone()).unwrap();
        assert_eq!(settings.get("domain"), None);

        settings.set("domain", "mvmo.dev").unwrap();
        settings.set("remember_domain", "never").unwrap();
        settings.set("domain", "acme.org").unwrap();

        let settings = Settings::load_from(file.clone()).unwrap();
        assert_eq!(
            (settings.get("domain"), settings.get("remember_domain")),
            (Some("acme.org"), Some("never"))
        );
        assert_eq!(
            system::read_file(&file).unwrap(),
            "# written by build config\ndomain = acme.org\nremember_domain = never\n"
        );

        let mut settings = Settings::load_from(file.clone()).unwrap();
        assert!(settings.remove("remember_domain").unwrap());
        assert!(!settings.remove("remember_domain").unwrap());
        assert_eq!(
            Settings::load_from(file.clone())
                .unwrap()
                .get("remember_domain"),
            None
        );

        system::write_file(&file, "broken").unwrap();
        assert!(
            Settings::load_from(file.clone())
                .err()
                .unwrap()
                .ends_with(":1: expected name = value")
        );

        std::fs::remove_file(&file).unwrap();
    }
}
