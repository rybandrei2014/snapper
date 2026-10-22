use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
        // Portable default: a work folder next to snapper.exe. Falls back to the
        // current directory if the exe path can't be resolved.
        let output_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("output");
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
        // Portable config: prefer `config.json` next to the exe; fall back to
        // `%APPDATA%\Snapper` when that folder isn't writable (e.g. Program Files).
        // A legacy `%APPDATA%` config is loaded and migrated to the exe folder.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()));
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("Snapper");

        let exe_cfg = exe_dir.as_ref().map(|d| d.join("config.json"));
        let appdata_cfg = appdata.join("config.json");

        let read_path = exe_cfg
            .as_ref()
            .filter(|p| p.exists())
            .cloned()
            .or_else(|| appdata_cfg.exists().then_some(appdata_cfg));

        let write_dir = exe_dir.filter(|d| writable(d)).unwrap_or_else(|| {
            let _ = std::fs::create_dir_all(&appdata);
            appdata
        });
        let write_path = write_dir.join("config.json");

        let cfg = match &read_path {
            Some(p) => {
                let text = std::fs::read_to_string(p)?;
                serde_json::from_str::<Config>(&text)?
            }
            None => Config::default(),
        };

        // Persist to the portable location (also migrates a legacy %APPDATA% config).
        if read_path.as_ref() != Some(&write_path) {
            if let Ok(text) = serde_json::to_string_pretty(&cfg) {
                let _ = std::fs::write(&write_path, text);
            }
        }
        Ok(cfg)
    }
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".snapper_write_test");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}
