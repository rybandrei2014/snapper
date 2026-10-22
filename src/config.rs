use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: String,
    pub audio: bool,
    pub mic: bool,
    pub audio_bitrate_kbps: u32,
    pub output_dir: PathBuf,
    pub screenshot_dir: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub output_idx_override: Option<i32>,
}

impl Default for Config {
    fn default() -> Self {
        let output_dir = std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("Videos")
            .join("Snapper");
        Config {
            fps: 60,
            bitrate_kbps: 12000,
            encoder: "auto".into(),
            audio: true,
            mic: false,
            audio_bitrate_kbps: 192,
            output_dir,
            screenshot_dir: None,
            ffmpeg_path: None,
            output_idx_override: None,
        }
    }
}

impl Config {
    pub fn screenshot_dir(&self) -> PathBuf {
        self.screenshot_dir.clone().unwrap_or_else(|| self.output_dir.clone())
    }
    pub fn load() -> Result<Config> {
        let dir = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("Snapper");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("config.json");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let cfg: Config = serde_json::from_str(&text)?;
            Ok(cfg)
        } else {
            let cfg = Config::default();
            if let Ok(text) = serde_json::to_string_pretty(&cfg) {
                let _ = std::fs::write(&path, text);
            }
            Ok(cfg)
        }
    }
}
