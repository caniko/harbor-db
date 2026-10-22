//! Give supervisor fixtures real trusted ancestry when a sandbox remaps `/`.
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Command,
};

pub fn enter() -> bool {
    if std::env::var_os("HARBOR_SUPERVISOR_PRIVATE_ROOT_CHILD").is_some() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert_eq!(fs::metadata("/").unwrap().uid(), 0);
        assert!(Path::new(env!("CARGO_BIN_EXE_harbor-db-test")).is_file());
        return true;
    }
    let root_uid = fs::metadata("/").unwrap().uid();
    if (root_uid == 0 || root_uid == unsafe { libc::geteuid() })
        && std::env::var_os("HARBOR_DB_TEST_PRIVATE_ROOT").is_none()
    {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_mount = root.path().join(source.strip_prefix("/").unwrap());
    // Workspace and external Cargo targets live outside the component source.
    // Preserve the compile-time launcher path inside the private root as well.
    let binaries = Path::new(env!("CARGO_BIN_EXE_harbor-db-test"))
        .parent()
        .unwrap();
    let binaries_mount = root.path().join(binaries.strip_prefix("/").unwrap());
    for path in [
        "nix/store",
        "proc",
        "dev",
        "bin",
        "etc",
        "run/binfmt",
        "run/supervisor-tmp",
        "tmp",
    ] {
        fs::create_dir_all(root.path().join(path)).unwrap();
    }
    fs::set_permissions(
        root.path().join("run/supervisor-tmp"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    // Retained executor environments intentionally omit TMPDIR. Native Cargo
    // and rustc must still have a private default temporary directory.
    fs::set_permissions(root.path().join("tmp"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        root.path().join("etc/passwd"),
        "root:x:0:0:fixture:/run/supervisor-tmp:/bin/sh\n",
    )
    .unwrap();
    fs::write(root.path().join("etc/group"), "root:x:0:\n").unwrap();
    fs::create_dir_all(&source_mount).unwrap();
    fs::create_dir_all(&binaries_mount).unwrap();
    // Empty retained environments deliberately use exec's default /bin lookup.
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("sh"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(shell, root.path().join("bin/sh")).unwrap();
    fs::copy(
        std::env::current_exe().unwrap(),
        root.path().join("fixture"),
    )
    .unwrap();
    let thread = std::thread::current();
    let test = thread
        .name()
        .expect("fixture must run on a named test thread");
    let result = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--", "sh", "-ec"])
        .arg(
            r#"
mount --rbind /nix/store "$1/nix/store"
mount --rbind /proc "$1/proc"
mount --rbind /dev "$1/dev"
mount --rbind "$2" "$3"
mount --rbind "$5" "$6"
mountpoint -q "$1/nix/store"
mountpoint -q "$3"
mountpoint -q "$6"
if [ -d /run/binfmt ]; then
    mount --rbind /run/binfmt "$1/run/binfmt"
    mountpoint -q "$1/run/binfmt"
fi
export TMPDIR=/run/supervisor-tmp
exec chroot "$1" /fixture --exact "$4" --nocapture
"#,
        )
        .arg("private-supervisor-root")
        .arg(root.path())
        .arg(source)
        .arg(source_mount)
        .arg(test)
        .arg(binaries)
        .arg(binaries_mount)
        .env("HARBOR_SUPERVISOR_PRIVATE_ROOT_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "{test}: {stdout}{}",
        String::from_utf8_lossy(&result.stderr)
    );
    false
}
