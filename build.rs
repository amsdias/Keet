fn main() {
    // Emit GIT_VERSION from `git describe --tags --always`, falling back to Cargo version.
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--always"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=GIT_VERSION={version}");
    // Re-run when the version can change: HEAD moving to another branch or
    // commit, the current branch getting a new commit (its ref file — HEAD
    // itself does not change on a commit, so watching only HEAD left a stale
    // version in every local build), a tag, or refs being packed.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    if let Some(branch) = std::fs::read_to_string(".git/HEAD")
        .ok()
        .and_then(|h| h.strip_prefix("ref: ").map(|r| r.trim().to_string()))
    {
        let ref_file = format!(".git/{branch}");
        if std::path::Path::new(&ref_file).exists() {
            println!("cargo:rerun-if-changed={ref_file}");
        }
    }
    if std::path::Path::new(".git/packed-refs").exists() {
        println!("cargo:rerun-if-changed=.git/packed-refs");
    }

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Keet");
        res.set("FileDescription", "Keet Audio Player");
        // Override file/product version with the git tag so Windows file properties
        // show the correct version instead of the stale Cargo.toml value.
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        res.compile().expect("Failed to compile Windows resources");
    }
}
