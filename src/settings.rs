use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Persistent appearance choice shared by the desktop and the headless agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceTheme {
    #[default]
    Light,
    Dark,
    System,
}

impl AppearanceTheme {
    pub fn as_str(self) -> &'static str {
        match self { Self::Light => "light", Self::Dark => "dark", Self::System => "system" }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Window-only preferences, intentionally stored outside workspace.json:
/// headless note agents do not need to understand or overwrite UI settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub sidebar_collapsed: bool,
    pub theme: AppearanceTheme,
}

struct SettingsLock(File);
impl Drop for SettingsLock {
    fn drop(&mut self) { let _ = self.0.unlock(); }
}

impl UiSettings {
    pub fn path_for(data_path: &Path) -> PathBuf {
        data_path.with_file_name("ui-settings.json")
    }

    pub fn load(data_path: &Path) -> io::Result<Self> {
        let _lock = Self::acquire_lock(data_path)?;
        Self::load_unlocked(data_path)
    }

    fn load_unlocked(data_path: &Path) -> io::Result<Self> {
        let path = Self::path_for(data_path);
        let backup = path.with_extension("json.bak");
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(settings) => Ok(settings),
                Err(primary_error) => fs::read(&backup).ok()
                    .and_then(|previous| serde_json::from_slice(&previous).ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData,
                        format!("Invalid UI settings and backup: {primary_error}"))),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Windows replacement needs a brief remove/rename window.
                // The backup is still a valid snapshot if a crash struck
                // between those operations.
                match fs::read(backup) {
                    Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error|
                        io::Error::new(io::ErrorKind::InvalidData, error)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, data_path: &Path) -> io::Result<()> {
        let _lock = Self::acquire_lock(data_path)?;
        self.save_unlocked(data_path)
    }

    /// Read-modify-write under one lock so an agent changing the theme does
    /// not race a desktop sidebar toggle and silently lose either preference.
    pub fn update(data_path: &Path, change: impl FnOnce(&mut Self)) -> io::Result<Self> {
        let _lock = Self::acquire_lock(data_path)?;
        let mut settings = Self::load_unlocked(data_path)?;
        change(&mut settings);
        settings.save_unlocked(data_path)?;
        Ok(settings)
    }

    fn acquire_lock(data_path: &Path) -> io::Result<SettingsLock> {
        let lock = Self::path_for(data_path).with_extension("json.lock");
        let file = OpenOptions::new().read(true).write(true).create(true).open(lock)?;
        file.lock()?;
        Ok(SettingsLock(file))
    }

    fn save_unlocked(&self, data_path: &Path) -> io::Result<()> {
        let path = Self::path_for(data_path);
        let temporary = path.with_extension("json.tmp");
        let mut file = File::create(&temporary)?;
        let encoded = serde_json::to_vec_pretty(self)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        // Preserve a readable previous snapshot if a Windows filesystem does
        // not support replace-on-rename. This preference contains no note data.
        if path.exists() {
            let previous = path.with_extension("json.bak");
            fs::copy(&path, previous)?;
            fs::remove_file(&path)?;
        }
        fs::rename(temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sidebar_choice_persists_independently_of_notes() {
        let root = std::env::temp_dir().join(format!(
            "inkpad-ui-settings-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("workspace.json");
        assert_eq!(UiSettings::load(&file).unwrap(), UiSettings::default());
        UiSettings { sidebar_collapsed: true, ..UiSettings::default() }.save(&file).unwrap();
        assert!(UiSettings::load(&file).unwrap().sidebar_collapsed);
        UiSettings { sidebar_collapsed: false, ..UiSettings::default() }.save(&file).unwrap();
        assert!(!UiSettings::load(&file).unwrap().sidebar_collapsed);
        assert!(!file.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn theme_round_trips_and_update_preserves_sidebar() {
        let root = std::env::temp_dir().join(format!("inkpad-theme-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("workspace.json");
        UiSettings::update(&file, |settings| settings.sidebar_collapsed = true).unwrap();
        let next = UiSettings::update(&file, |settings| settings.theme = AppearanceTheme::Dark).unwrap();
        assert!(next.sidebar_collapsed);
        assert_eq!(next.theme, AppearanceTheme::Dark);
        assert_eq!(UiSettings::load(&file).unwrap().theme.as_str(), "dark");
        assert_eq!(AppearanceTheme::parse("SYSTEM"), Some(AppearanceTheme::System));
        assert_eq!(AppearanceTheme::parse("invalid"), None);
        fs::remove_dir_all(root).unwrap();
    }
}
