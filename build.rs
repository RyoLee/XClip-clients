//! Build script: embed the application manifest and the icon resources.
//!
//! The manifest makes the process per-monitor DPI aware and keeps it at
//! asInvoker, so no UAC prompt appears and the tray icon is not blurry.
//! The icons are the artwork in assets/: the .ico becomes the executable
//! icon (Explorer, taskbar, Alt-Tab) and a small PNG is embedded so the
//! tray can load the same art instead of the generic system icon.
//!
//! Both steps need the MSVC resource compiler. If it is missing we warn
//! loudly rather than silently shipping an unmanifested icon-less binary.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=xclip.exe.manifest");
    println!("cargo:rerun-if-changed=assets/xclip.ico");
    println!("cargo:rerun-if-changed=assets/xclip-paused.ico");

    #[cfg(windows)]
    {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = root.join("xclip.exe.manifest");
        let ico = root.join("assets").join("xclip.ico");
        let ico_paused = root.join("assets").join("xclip-paused.ico");

        let Some(rc) = find_rc() else {
            println!("cargo:warning=rc.exe not found; xclip.exe will be built WITHOUT an embedded manifest or icon");
            return;
        };

        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let rc_file = out.join("xclip.rc");
        let res_file = out.join("xclip.res");

        // Resource ids: 1 = icon, 24 (RT_MANIFEST) = application manifest.
        let mut script = String::new();
        if ico.exists() {
            let p = ico.to_string_lossy().replace('\\', "/");
            // Id 1 is the application icon the shell picks up for the .exe.
            script.push_str(&format!("1 ICON \"{p}\"\n"));
        } else {
            println!("cargo:warning=assets/xclip.ico missing; executable will have no icon");
        }
        if ico_paused.exists() {
            let p = ico_paused.to_string_lossy().replace('\\', "/");
            // Id 2 is the paused-state variant the tray swaps in.
            script.push_str(&format!("2 ICON \"{p}\"\n"));
        } else {
            println!("cargo:warning=assets/xclip-paused.ico missing; no paused icon");
        }
        if manifest.exists() {
            let p = manifest.to_string_lossy().replace('\\', "/");
            script.push_str(&format!("1 24 \"{p}\"\n"));
        } else {
            println!("cargo:warning=manifest missing; executable will not be DPI aware");
        }

        if let Err(e) = std::fs::write(&rc_file, script) {
            println!("cargo:warning=could not write rc script: {e}");
            return;
        }

        let status = std::process::Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res_file)
            .arg(&rc_file)
            .status();

        match status {
            Ok(s) if s.success() => println!("cargo:rustc-link-arg={}", res_file.display()),
            Ok(s) => println!("cargo:warning=rc.exe failed with {s}; resources not embedded"),
            Err(e) => println!("cargo:warning=could not run rc.exe: {e}"),
        }
    }
}

/// Locate the MSVC resource compiler, preferring the host architecture.
#[cfg(windows)]
fn find_rc() -> Option<std::path::PathBuf> {
    // 1. Whatever the developer environment already exposes (VS dev prompt).
    if let Some(p) = on_path("rc.exe") {
        return Some(p);
    }

    // 2. Any installed Windows SDK, newest first, host architecture first.
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let host_arches: [&str; 2] = if cfg!(target_arch = "aarch64") {
        ["arm64", "x64"]
    } else if cfg!(target_pointer_width = "64") {
        ["x64", "x86"]
    } else {
        ["x86", "x64"]
    };

    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        let mut versions: Vec<_> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        // SDK directory names are version strings; sort so 10.0.26100 beats 10.0.17763.
        versions.sort_by(|a, b| version_key(b).cmp(&version_key(a)));

        for v in versions {
            for arch in host_arches {
                let cand = v.join(arch).join("rc.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn version_key(p: &std::path::Path) -> Vec<u32> {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.split('.')
                .filter_map(|s| s.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(windows)]
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file())
}
