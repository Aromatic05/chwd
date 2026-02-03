use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const LOCK_PATH: &str = "/run/nv-helper.lock";
const SRC_ROOT: &str = "/var/cache/nv-helper/src";
const PKG_ROOT: &str = "/var/cache/nv-helper/pkg";
const BUILD_ROOT: &str = "/var/lib/nv-helper/build";
const HOME_ROOT: &str = "/var/lib/nv-helper/home";
const BUILD_USER: &str = "nvbuild";

fn main() {
    if let Err(err) = run() {
        eprintln!("ERROR: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let branch = parse_args()?;
    ensure_root()?;

    let _lock = LockGuard::acquire(LOCK_PATH)?;

    ensure_dirs()?;
    // Remove conflicting official NVIDIA packages early
    pacman_remove_rdd(&["nvidia-open-dkms", "nvidia-utils"])?;

    let multilib = multilib_available()?;
    let repos = branch_repos(branch);

    let _build_user = BuildUserGuard::ensure()?;
    chown_path(Path::new(BUILD_ROOT), BUILD_USER)?;
    chown_path(Path::new(PKG_ROOT), BUILD_USER)?;
    chown_path(Path::new(HOME_ROOT), BUILD_USER)?;

    install_prereqs()?;

    let mut built_packages = Vec::new();
    for repo in repos {
        let src_dir = ensure_aur_repo(repo)?;
        let build_dir = Path::new(BUILD_ROOT).join(repo);
        let _build_guard = BuildDirGuard::new(build_dir.clone());

        if build_dir.exists() {
            fs::remove_dir_all(&build_dir)
                .map_err(|e| format!("failed to clean build dir {build_dir:?}: {e}"))?;
        }
        fs::create_dir_all(&build_dir)
            .map_err(|e| format!("failed to create build dir {build_dir:?}: {e}"))?;

        rsync_repo(&src_dir, &build_dir)?;
        chown_recursive(&build_dir, BUILD_USER)?;

        install_repo_deps(repo)?;
        build_pkg(&build_dir)?;
        let pkgs = list_built_packages(&build_dir)?;

        let filtered_pkgs = filter_multilib_packages(pkgs, multilib);
        if !filtered_pkgs.is_empty() {
            pacman_install_built(&filtered_pkgs)?;
        }

        built_packages.extend(filtered_pkgs);
    }

    if built_packages.is_empty() {
        return Err("no packages were built".to_string());
    }

    println!("==> All packages built and installed successfully!");

    Ok(())
}

fn parse_args() -> Result<&'static str, String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: nv-helper <580xx|470xx>".to_string());
    }
    match args[1].as_str() {
        "580xx" => Ok("580xx"),
        "470xx" => Ok("470xx"),
        _ => Err("nv-helper supports only 580xx or 470xx".to_string()),
    }
}

fn ensure_root() -> Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Err("nv-helper must be run as root".to_string());
    }
    Ok(())
}

struct LockGuard {
    _file: fs::File,
}

impl LockGuard {
    fn acquire(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(|e| format!("failed to open lock file {path}: {e}"))?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(format!("failed to acquire lock on {path}"));
        }
        Ok(Self { _file: file })
    }
}

struct BuildUserGuard {
    created_by_us: bool,
}

impl BuildUserGuard {
    fn ensure() -> Result<Self, String> {
        if user_exists(BUILD_USER)? {
            return Ok(Self { created_by_us: false });
        }
        let mut cmd = Command::new("useradd");
        cmd.arg("--system")
            .arg("--user-group")
            .arg("--home-dir")
            .arg(format!("/var/lib/nv-helper/{BUILD_USER}"))
            .arg("--shell")
            .arg("/usr/bin/nologin")
            .arg(BUILD_USER);
        run_cmd_capture(&mut cmd, "useradd")?;
        Ok(Self { created_by_us: true })
    }
}

impl Drop for BuildUserGuard {
    fn drop(&mut self) {
        if !self.created_by_us {
            return;
        }
        let _ = Command::new("userdel").arg("-r").arg(BUILD_USER).output();
    }
}

struct BuildDirGuard {
    path: PathBuf,
}

impl BuildDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for BuildDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn user_exists(name: &str) -> Result<bool, String> {
    let status = Command::new("id")
        .arg("-u")
        .arg(name)
        .status()
        .map_err(|e| format!("failed to check user {name}: {e}"))?;
    Ok(status.success())
}

fn multilib_available() -> Result<bool, String> {
    let status = Command::new("pacman")
        .arg("-Si")
        .arg("lib32-glibc")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to check multilib availability: {e}"))?;
    Ok(status.success())
}

fn branch_repos(branch: &str) -> Vec<&'static str> {
    match branch {
        "580xx" => vec!["nvidia-580xx-utils", "nvidia-580xx-settings"],
        _ => vec!["nvidia-470xx-utils", "nvidia-470xx-settings"],
    }
}

fn ensure_dirs() -> Result<(), String> {
    fs::create_dir_all(SRC_ROOT).map_err(|e| format!("failed to create {SRC_ROOT}: {e}"))?;
    fs::create_dir_all(PKG_ROOT).map_err(|e| format!("failed to create {PKG_ROOT}: {e}"))?;
    fs::create_dir_all(BUILD_ROOT).map_err(|e| format!("failed to create {BUILD_ROOT}: {e}"))?;
    fs::create_dir_all(HOME_ROOT).map_err(|e| format!("failed to create {HOME_ROOT}: {e}"))?;
    Ok(())
}

fn repo_deps(repo: &str) -> Vec<&'static str> {
    if repo.contains("utils") {
        vec!["libglvnd", "egl-wayland", "egl-gbm", "egl-x11"]
    } else if repo.contains("settings") {
        vec!["jansson", "gtk3", "libxv", "libvdpau", "libxext", "vulkan-headers"]
    } else {
        vec![]
    }
}

fn prereqs() -> Vec<&'static str> {
    vec!["git", "base-devel", "dkms", "rsync"]
}

fn install_prereqs() -> Result<(), String> {
    let packages = prereqs().into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
    pacman_install_stream(&packages)
}

fn install_repo_deps(repo: &str) -> Result<(), String> {
    let mut all_deps = prereqs().into_iter().collect::<Vec<_>>();
    all_deps.extend(repo_deps(repo));
    let packages = all_deps.into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
    pacman_install_stream(&packages)
}

fn ensure_aur_repo(repo: &str) -> Result<PathBuf, String> {
    let src_dir = Path::new(SRC_ROOT).join(repo);
    if src_dir.exists() {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&src_dir).arg("reset").arg("--hard");
        run_cmd_stream(&mut cmd, &format!("git reset --hard in {repo}"))?;
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&src_dir).arg("clean").arg("-fdx");
        run_cmd_stream(&mut cmd, &format!("git clean -fdx in {repo}"))?;
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&src_dir).arg("pull").arg("--ff-only");
        run_cmd_stream(&mut cmd, &format!("git pull {repo}"))?;
    } else {
        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(format!("https://aur.archlinux.org/{repo}.git")).arg(&src_dir);
        run_cmd_stream(&mut cmd, &format!("git clone {repo}"))?;
    }
    Ok(src_dir)
}

fn rsync_repo(src_dir: &Path, build_dir: &Path) -> Result<(), String> {
    let src = format!("{}/", src_dir.display());
    let dst = format!("{}/", build_dir.display());
    let mut cmd = Command::new("rsync");
    cmd.arg("-a").arg("--delete").arg("--exclude").arg(".git").arg(src).arg(dst);
    let repo_name = build_dir.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    run_cmd_stream(&mut cmd, &format!("rsync {repo_name}"))?;
    Ok(())
}

fn build_pkg(build_dir: &Path) -> Result<(), String> {
    let pkgdest = format!("PKGDEST={PKG_ROOT}");
    let home = format!("HOME={HOME_ROOT}");
    let mut cmd = Command::new("runuser");
    cmd.arg("-u")
        .arg(BUILD_USER)
        .arg("--")
        .arg("env")
        .arg(&pkgdest)
        .arg(&home)
        .arg("makepkg")
        .arg("-f")
        .arg("--noconfirm")
        .arg("--needed");
    cmd.current_dir(build_dir);
    let repo_name = build_dir.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    run_cmd_stream(&mut cmd, &format!("makepkg {repo_name}"))?;
    Ok(())
}

fn list_built_packages(build_dir: &Path) -> Result<Vec<String>, String> {
    let pkgdest = format!("PKGDEST={PKG_ROOT}");
    let home = format!("HOME={HOME_ROOT}");
    let mut cmd = Command::new("runuser");
    cmd.arg("-u")
        .arg(BUILD_USER)
        .arg("--")
        .arg("env")
        .arg(&pkgdest)
        .arg(&home)
        .arg("makepkg")
        .arg("--packagelist");
    cmd.current_dir(build_dir);
    let output = run_cmd_capture(&mut cmd, "makepkg --packagelist")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pkgs = stdout
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    Ok(pkgs)
}

fn filter_multilib_packages(pkgs: Vec<String>, multilib: bool) -> Vec<String> {
    if multilib {
        return pkgs;
    }
    pkgs.into_iter()
        .filter(|pkg| {
            let filename = Path::new(pkg).file_name().and_then(|name| name.to_str()).unwrap_or("");
            !filename.starts_with("lib32-")
        })
        .collect()
}

fn pacman_install_built(pkgfiles: &[String]) -> Result<(), String> {
    if pkgfiles.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("pacman");
    cmd.arg("-U").arg("--noconfirm").arg("--needed");
    for pkg in pkgfiles {
        cmd.arg(pkg);
    }
    run_cmd_stream(&mut cmd, "Installing built packages (pacman -U)")?;
    Ok(())
}

fn is_installed(pkg: &str) -> Result<bool, String> {
    let status = Command::new("pacman")
        .arg("-Qi")
        .arg(pkg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to check installed package {pkg}: {e}"))?;
    Ok(status.success())
}

fn pacman_remove_rdd(pkgs: &[&str]) -> Result<(), String> {
    let mut to_remove = Vec::new();
    for &p in pkgs {
        if is_installed(p)? {
            to_remove.push(p);
        }
    }
    if to_remove.is_empty() {
        println!("==> No conflicting packages to remove");
        return Ok(());
    }
    let mut cmd = Command::new("pacman");
    cmd.arg("-Rdd").arg("--noconfirm");
    for p in &to_remove {
        cmd.arg(p);
    }
    run_cmd_stream(&mut cmd, "Removing conflicting packages (pacman -Rdd)")?;
    Ok(())
}

fn pacman_install(packages: &[String]) -> Result<(), String> {
    if packages.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("pacman");
    cmd.arg("--noconfirm").arg("--needed").arg("-S");
    for pkg in packages {
        cmd.arg(pkg);
    }
    run_cmd_capture(&mut cmd, "pacman -S")?;
    Ok(())
}

fn pacman_install_stream(packages: &[String]) -> Result<(), String> {
    if packages.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("pacman");
    cmd.arg("--noconfirm").arg("--needed").arg("-Syu");
    for pkg in packages {
        cmd.arg(pkg);
    }
    run_cmd_stream(&mut cmd, "Installing dependencies")?;
    Ok(())
}

fn run_cmd_stream(cmd: &mut Command, desc: &str) -> Result<(), String> {
    println!("==> {desc}");
    let status = cmd.status().map_err(|e| format!("{desc} failed to execute: {e}"))?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(format!("{desc} failed with exit code {code}"));
    }
    Ok(())
}

fn run_cmd_capture(cmd: &mut Command, desc: &str) -> Result<Output, String> {
    let output = cmd.output().map_err(|e| format!("{desc} failed to execute: {e}"))?;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{desc} failed with exit code {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ));
    }
    Ok(output)
}

fn chown_path(path: &Path, user: &str) -> Result<(), String> {
    let mut cmd = Command::new("chown");
    cmd.arg(format!("{user}:{user}")).arg(path);
    run_cmd_capture(&mut cmd, "chown")?;
    Ok(())
}

fn chown_recursive(path: &Path, user: &str) -> Result<(), String> {
    let mut cmd = Command::new("chown");
    cmd.arg("-R").arg(format!("{user}:{user}")).arg(path);
    run_cmd_capture(&mut cmd, "chown")?;
    Ok(())
}

fn dedup_packages(pkgs: Vec<String>) -> Vec<String> {
    let mut set = BTreeSet::new();
    for pkg in pkgs {
        set.insert(pkg);
    }
    set.into_iter().collect()
}
