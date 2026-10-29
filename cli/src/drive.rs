//! Finds the BOOTSEL mass storage drive (the one with INFO_UF2.TXT).

use std::fs;
use std::path::PathBuf;

pub struct BootDrive {
    pub root: PathBuf,
    /// The `Model:` line, e.g. "Raspberry Pi RP2350"
    pub model: String,
}

fn candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(windows) {
        for letter in b'C'..=b'Z' {
            roots.push(PathBuf::from(format!("{}:\\", letter as char)));
        }
    } else {
        let user = std::env::var("USER").unwrap_or_default();
        let parents = [
            format!("/media/{user}"),
            format!("/run/media/{user}"),
            "/media".to_string(),
            "/Volumes".to_string(),
        ];
        for parent in parents {
            if let Ok(entries) = fs::read_dir(parent) {
                roots.extend(entries.flatten().map(|e| e.path()));
            }
        }
    }
    roots
}

pub fn find() -> Option<BootDrive> {
    candidates().into_iter().find_map(|root| {
        let info = fs::read_to_string(root.join("INFO_UF2.TXT")).ok()?;
        if !(info.contains("RP2350") || info.contains("RP2040")) {
            return None;
        }
        let model = info
            .lines()
            .find_map(|l| l.strip_prefix("Model:"))
            .unwrap_or("")
            .trim()
            .to_string();
        Some(BootDrive { root, model })
    })
}
