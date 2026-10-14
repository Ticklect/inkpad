use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Window-only preferences, intentionally stored outside workspace.json:
/// headless note agents do not need to understand or overwrite UI settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub sidebar_collapsed: bool,
}

impl UiSettings {
    pub fn path_for(data_path: &Path) -> PathBuf {
        data_path.with_file_name("ui-settings.json")
    }

    pub fn load(data_path: &Path) -> io::Result<Self> {
        let path = Self::path_for(data_path);
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("Invalid UI settings: {error}"))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, data_path: &Path) -> io::Result<()> {
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
        UiSettings { sidebar_collapsed: true }.save(&file).unwrap();
        assert!(UiSettings::load(&file).unwrap().sidebar_collapsed);
        UiSettings { sidebar_collapsed: false }.save(&file).unwrap();
        assert!(!UiSettings::load(&file).unwrap().sidebar_collapsed);
        assert!(!file.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
