fn main() {
    slint_build::compile("ui/main.slint").expect("Slint UI failed to compile");
    println!("cargo:rerun-if-changed=assets/inkpad.ico");
    println!("cargo:rerun-if-changed=assets/inkpad.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

/// Build the native PE .rsrc icon with the Windows SDK resource compiler.
/// This deliberately has no crate dependencies, remains buildable offline, and
/// affects only the GUI executable. The stdio agent retains its console icon.
fn embed_windows_icon() {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("assets");
    validate_icon(&source.join("inkpad.ico"));
    let destination = PathBuf::from(std::env::var_os("OUT_DIR").expect("build output dir"))
        .join("inkpad-icon.res");

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let sdk_arch = match target_arch.as_str() {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        _ => panic!("Windows icon compilation is unsupported for architecture {target_arch}"),
    };

    // The SDK typically installs rc.exe outside PATH; search Windows Kits.
    let mut compilers = vec![PathBuf::from("rc.exe")];
    if let Some(sdk_dir) = std::env::var_os("WindowsSdkDir") {
        add_sdk_compilers(&mut compilers, Path::new(&sdk_dir).join("bin"), sdk_arch);
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles(x86)") {
        let kits = Path::new(&program_files).join("Windows Kits");
        add_sdk_compilers(&mut compilers, kits.join("10").join("bin"), sdk_arch);
        add_sdk_compilers(&mut compilers, kits.join("8.1").join("bin"), sdk_arch);
    }

    let mut errors = Vec::new();
    for compiler in compilers {
        let result = Command::new(&compiler)
            .current_dir(&source)
            .arg("/nologo")
            .arg(format!("/fo{}", destination.display()))
            .arg("inkpad.rc")
            .output();
        match result {
            Ok(output) if output.status.success() && destination.exists() => {
                println!("cargo:rustc-link-arg-bin=inkpad={}", destination.display());
                return;
            }
            Ok(output) => errors.push(format!("{}: {} {}", compiler.display(),
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))),
            Err(error) => errors.push(format!("{}: {error}", compiler.display())),
        }
    }
    panic!("Could not compile Inkpad's Windows icon resource; install Windows SDK rc.exe. Attempts:\n{}", errors.join("\n"));
}

/// Keep the taskbar/DPI size coverage from accidentally regressing if the
/// image file is replaced. ICO dimensions store 256 as a zero-byte width.
fn validate_icon(path: &std::path::Path) {
    let data = std::fs::read(path).expect("Inkpad ICO must be present");
    assert!(data.len() >= 6 && data[0..4] == [0, 0, 1, 0],
        "Inkpad icon must be a valid ICO file");
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    assert!(count >= 6 && data.len() >= 6 + count * 16,
        "Inkpad icon must contain at least six image entries");
    let mut sizes = Vec::new();
    for index in 0..count {
        let entry = &data[6 + index * 16..6 + (index + 1) * 16];
        let width = if entry[0] == 0 { 256 } else { entry[0] as usize };
        let height = if entry[1] == 0 { 256 } else { entry[1] as usize };
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        assert!(size >= 8 && offset.checked_add(size).is_some_and(|end| end <= data.len())
            && data[offset..offset + 8] == [137, 80, 78, 71, 13, 10, 26, 10],
            "Inkpad ICO entry {index} must contain a complete PNG payload");
        assert_eq!(width, height, "Inkpad ICO must use square frames");
        sizes.push(width);
    }
    for requested in [16, 24, 32, 48, 64, 256] {
        assert!(sizes.contains(&requested), "Inkpad ICO is missing the {requested}px frame");
    }
}

fn add_sdk_compilers(compilers: &mut Vec<std::path::PathBuf>, bin_dir: std::path::PathBuf, arch: &str) {
    if !bin_dir.is_dir() { return; }
    let direct = bin_dir.join(arch).join("rc.exe");
    if direct.exists() { compilers.push(direct); }
    let mut versions = match std::fs::read_dir(&bin_dir) {
        Ok(entries) => entries.filter_map(Result::ok).map(|entry| entry.path())
            .filter(|path| path.is_dir()).collect::<Vec<_>>(),
        Err(_) => return,
    };
    versions.sort_by(|a, b| b.cmp(a));
    for version in versions {
        let compiler = version.join(arch).join("rc.exe");
        if compiler.exists() { compilers.push(compiler); }
    }
}
