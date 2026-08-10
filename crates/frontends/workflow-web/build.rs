use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    let Some(ffmpeg_dir) = env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
        return;
    };
    let runtime_dir = ffmpeg_dir.join("runtime");
    println!("cargo:rerun-if-changed={}", runtime_dir.display());
    if !runtime_dir.is_dir() {
        return;
    }

    let Ok(profile_dir) = profile_target_dir() else {
        return;
    };
    if let Err(error) = copy_runtime_libraries(&runtime_dir, &profile_dir) {
        println!(
            "cargo:warning=failed to copy FFmpeg runtime libraries from {} to {}: {}",
            runtime_dir.display(),
            profile_dir.display(),
            error
        );
    }
}

fn profile_target_dir() -> io::Result<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "OUT_DIR is not set for build script",
        )
    })?);
    // OUT_DIR 形如 target/debug/build/<pkg-hash>/out；可执行文件位于 target/debug。
    out_dir
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other(format!("invalid OUT_DIR: {}", out_dir.display())))
}

fn copy_runtime_libraries(runtime_dir: &Path, target_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(target_dir)?;
    for entry in fs::read_dir(runtime_dir)? {
        let entry = entry?;
        let source = entry.path();
        if !source.is_file() {
            continue;
        }
        let Some(name) = source.file_name() else {
            continue;
        };
        fs::copy(&source, target_dir.join(name))?;
    }
    Ok(())
}
