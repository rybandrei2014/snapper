// Embeds assets\icon.ico as the exe icon (shown in Explorer, taskbar, task
// manager). Compiled with the Windows SDK rc.exe; if the SDK is missing the
// exe simply keeps the default icon (also lets non-Windows hosts still build).

use std::path::{Path, PathBuf};

fn find_rc() -> Option<PathBuf> {
    let prog_x86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| String::new());
    let prog = std::env::var("ProgramFiles").unwrap_or_else(|_| String::new());
    let kits: Vec<PathBuf> = [prog_x86, prog]
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| Path::new(p).join(r"Windows Kits\10\bin"))
        .filter(|p| p.is_dir())
        .collect();
    let mut versions: Vec<(u64, u64, u64, u64, PathBuf)> = Vec::new();
    for kit in kits {
        let Ok(rd) = std::fs::read_dir(&kit) else { continue };
        for entry in rd.flatten() {
            let exe = entry.path().join(r"x64\rc.exe");
            if exe.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let mut parts = [0u64; 4];
                for (i, p) in name.split('.').take(4).enumerate() {
                    parts[i] = p.parse().unwrap_or(0);
                }
                versions.push((parts[0], parts[1], parts[2], parts[3], exe));
            }
        }
    }
    versions.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
    versions.last().map(|v| v.4.clone())
}

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico = manifest.join(r"assets\icon.ico");
    let rc = manifest.join("app.rc");
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed={}", rc.display());

    if std::env::consts::OS != "windows" {
        return;
    }
    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=rc.exe not found in the Windows SDK; exe icon not embedded");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("snapper.res");
    let status = std::process::Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&out)
        .arg(&rc)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", out.display());
        }
        Ok(_) => println!("cargo:warning=rc.exe failed; exe icon not embedded"),
        Err(e) => println!("cargo:warning=rc.exe failed to run ({e}); exe icon not embedded"),
    }
}
