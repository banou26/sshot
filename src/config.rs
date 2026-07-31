use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub save: SaveConfig,
    pub appearance: AppearanceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveConfig {
    /// Base directory for screenshots
    pub directory: String,
    /// Subfolder format (strftime). e.g. "%Y-%m"
    pub subfolder: String,
    /// Filename format for window captures.
    /// Variables: {title}, {random}. Also supports strftime.
    pub window_format: String,
    /// Filename format for region captures.
    /// Variables: {random}. Also supports strftime.
    pub region_format: String,
    /// File manager used to reveal a capture from the tray.
    pub file_manager: String,
    /// Argument that makes `file_manager` select a file rather than just open
    /// its directory. Empty means the directory is opened instead.
    pub file_manager_select_arg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub dim_factor: f32,
    pub border_width: u32,
    pub window_border_color: [u8; 3],
    pub region_border_color: [u8; 3],
    /// Fallback tiling gap, used only when niri's own config cannot be read.
    ///
    /// niri's `layout { gaps }` is the real setting and is read directly, so
    /// this no longer has to be kept in sync by hand. It defaults to niri's
    /// own default rather than to any particular setup.
    pub tiling_gap: u32,
}

impl Default for SaveConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self {
            directory: format!("{home}/Pictures/Screenshots"),
            subfolder: "%Y-%m".into(),
            window_format: "{title} %Y-%m-%d-%H-%M-%S-{random}".into(),
            region_format: "%Y-%m-%d-%H-%M-%S-{random}".into(),
            file_manager: "xdg-open".into(),
            file_manager_select_arg: String::new(),
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            dim_factor: 0.75,
            border_width: 3,
            window_border_color: [80, 140, 255],
            region_border_color: [255, 255, 255],
            tiling_gap: 16,
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("sshot")
            .join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(config) => config,
                Err(e) => {
                    // Never write defaults back over a file we failed to parse:
                    // that would silently destroy the user's settings because
                    // of a stray comma.
                    eprintln!("Warning: invalid config at {}: {e}", path.display());
                    eprintln!("Using defaults. The file was left untouched.");
                    Self::default()
                }
            },
            Err(_) => {
                // No config yet. Seed one, unless the path is nix-managed.
                let config = Self::default();
                if !path.exists() && !path.is_symlink() {
                    config.save_to_disk();
                }
                config
            }
        }
    }

    pub fn save_to_disk(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(content) = serde_json::to_string_pretty(self) else { return };
        // Write through a temporary file so an interrupted write cannot leave
        // a truncated config behind.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, content).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Maximum bytes of window title kept in a filename. Titles are arbitrary
    /// UTF-8, so this is a byte budget applied at a character boundary: a
    /// filename is bounded by NAME_MAX (255) in bytes, not in characters.
    const TITLE_BUDGET: usize = 80;

    /// Sanitize a window title into a filename fragment.
    pub fn sanitize_title(title: &str) -> String {
        let safe: String = title
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
            .collect();
        let safe = safe.trim().trim_matches('_').trim();

        let end = if safe.len() > Self::TITLE_BUDGET {
            // Truncating at a raw byte index panics mid-codepoint, which killed
            // the daemon on any title with non-ASCII letters past the budget.
            (0..=Self::TITLE_BUDGET)
                .rev()
                .find(|&i| safe.is_char_boundary(i))
                .unwrap_or(0)
        } else {
            safe.len()
        };

        let truncated = safe[..end].trim_end();
        if truncated.is_empty() { "window".to_string() } else { truncated.to_string() }
    }

    /// Render a strftime template, falling back to an ISO-ish default if the
    /// user's template has an invalid specifier. chrono panics when formatting
    /// a bad template, so it has to be validated before use.
    fn format_time(now: &chrono::DateTime<chrono::Local>, template: &str) -> String {
        if chrono::format::StrftimeItems::new(template).parse().is_err() {
            eprintln!("Warning: invalid filename template {template:?}, using default");
            return now.format("%Y-%m-%d-%H-%M-%S").to_string();
        }
        now.format(template).to_string()
    }

    pub fn format_filename(&self, window_title: Option<&str>) -> String {
        let now = chrono::Local::now();
        let rand_id: String = (0..6)
            .map(|_| {
                let i = rand::random::<u8>() % 36;
                if i < 10 { (b'0' + i) as char } else { (b'a' + i - 10) as char }
            })
            .collect();

        let template = match window_title {
            Some(_) => &self.save.window_format,
            None => &self.save.region_format,
        };

        let name = Self::format_time(&now, template).replace("{random}", &rand_id);
        let name = match window_title {
            Some(title) => name.replace("{title}", &Self::sanitize_title(title)),
            None => name,
        };

        format!("{name}.png")
    }

    /// Expand ~ to $HOME in a path string.
    pub fn expand_path(path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(rest)
        } else if path == "~" {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home)
        } else {
            PathBuf::from(path)
        }
    }

    pub fn save_dir(&self) -> Result<PathBuf> {
        let now = chrono::Local::now();
        let subfolder = Self::format_time(&now, &self.save.subfolder);
        let dir = Self::expand_path(&self.save.directory).join(subfolder);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_title_truncates_at_a_char_boundary() {
        // 27 x U+65E5 is 81 bytes, so a raw [..80] slice lands mid-codepoint.
        let title = "\u{65e5}".repeat(27);
        let out = Config::sanitize_title(&title);
        assert!(out.len() <= Config::TITLE_BUDGET);
        assert_eq!(out.chars().count(), 26);
    }

    #[test]
    fn sanitize_title_replaces_separators_and_trims() {
        assert_eq!(Config::sanitize_title("  a/b:c  "), "a_b_c");
        assert_eq!(Config::sanitize_title("keeps spaces"), "keeps spaces");
    }

    #[test]
    fn sanitize_title_falls_back_when_empty() {
        assert_eq!(Config::sanitize_title(""), "window");
        assert_eq!(Config::sanitize_title("///"), "window");
    }

    #[test]
    fn format_filename_is_bounded_and_ends_in_png() {
        let config = Config::default();
        let name = config.format_filename(Some(&"\u{65e5}".repeat(200)));
        assert!(name.ends_with(".png"));
        assert!(name.len() < 255, "filename must fit NAME_MAX: {}", name.len());
    }

    #[test]
    fn invalid_strftime_template_does_not_panic() {
        let mut config = Config::default();
        config.save.region_format = "%Q-invalid".into();
        let name = config.format_filename(None);
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn expand_path_handles_tilde() {
        std::env::set_var("HOME", "/home/test");
        assert_eq!(Config::expand_path("~/pics"), PathBuf::from("/home/test/pics"));
        assert_eq!(Config::expand_path("~"), PathBuf::from("/home/test"));
        assert_eq!(Config::expand_path("/abs"), PathBuf::from("/abs"));
        // A bare ~ without a separator is not a home reference.
        assert_eq!(Config::expand_path("~foo"), PathBuf::from("~foo"));
    }
}
