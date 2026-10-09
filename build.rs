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
    // itself does not change on a commit), a tag, or refs being packed. Paths
    // come from git itself (`rev-parse --git-path`): in a worktree or a
    // submodule `.git` is a file pointing elsewhere, and hard-coded `.git/…`
    // paths watched nothing there.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    //
    // A path that does not exist is not watched (cargo would rerun the
    // script on every build), with one exception: the branch's loose ref is
    // missing while the branch lives only in packed-refs (a fresh clone, a
    // `git gc`), or before its first commit — and the next commit CREATES
    // it. Its nearest existing directory is watched instead (refs/heads at
    // worst), so that creation is seen. A missing packed-refs needs nothing:
    // packing refs moves them, it never changes which commit is checked out.
    let mut watch = vec!["HEAD".to_string(), "refs/tags".to_string(), "packed-refs".to_string()];
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    if let Some(b) = &branch {
        watch.push(b.clone());
    }
    // Always: with no git (a source tarball) nothing below is printed, and a
    // build script that names no file is rerun on ANY change in the package.
    // The version then comes from Cargo.toml, which cargo tracks itself.
    println!("cargo:rerun-if-changed=build.rs");
    for item in watch {
        let Some(path) = git(&["rev-parse", "--git-path", &item]) else { continue };
        let mut path = std::path::PathBuf::from(path);
        if branch.as_deref() == Some(item.as_str()) {
            let heads = git(&["rev-parse", "--git-path", "refs/heads"]).map(std::path::PathBuf::from);
            while !path.exists() && Some(&path) != heads.as_ref() {
                if !path.pop() {
                    break;
                }
            }
        }
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // The icon and version resource, for a Windows TARGET built on a Windows
    // host (the resource compiler only runs there). `#[cfg]` alone says what
    // the build script runs on, not what it builds for: a Windows host
    // building for another OS embedded a Windows resource anyway.
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
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
