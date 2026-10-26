use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const TRY_SCRIPT: &str = include_str!("../assets/try");
const EXCLUDED: [&str; 5] = ["/proc", "/dev", "/sys", "/run/mount", "/run/try_trace.log"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Symlink,
    ReplacedDir,
    CreatedDir,
    Deleted,
    Modified,
    Added,
}

impl Kind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::ReplacedDir => "replaced with dir",
            Self::CreatedDir => "created dir",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Added => "added",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub kind: Kind,
    pub path: PathBuf,
    pub upper: PathBuf,
}

#[derive(Clone)]
pub struct Try {
    script: PathBuf,
    root: PathBuf,
}

impl Try {
    pub fn install(home: &Path) -> io::Result<Option<Self>> {
        remove_stale();
        if !cfg!(target_os = "linux") || !probe() {
            return Ok(None);
        }
        let directory = home.join("try");
        std::fs::create_dir_all(&directory)?;
        let script = directory.join(format!("try-{:016x}", fnv(TRY_SCRIPT.as_bytes())));
        if std::fs::read_to_string(&script).ok().as_deref() != Some(TRY_SCRIPT) {
            let partial = directory.join(format!(".try-{}", std::process::id()));
            std::fs::write(&partial, TRY_SCRIPT)?;
            std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
            std::fs::rename(&partial, &script)?;
        }
        let root = std::env::temp_dir().join(format!("nanocode-try-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        Ok(Some(Self { script, root }))
    }

    pub fn sandbox(&self, command: &str) -> io::Result<(Sandbox, String)> {
        let dir = self.root.join(uuid::Uuid::now_v7().simple().to_string());
        std::fs::create_dir(&dir)?;
        let script = dir.join("command.sh");
        std::fs::write(&script, command)?;
        let path = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|entry| !entry.starts_with("/run/wrappers"))
            .collect::<Vec<_>>()
            .join(":");
        let wrapped = format!(
            "PATH={path} sh {try_script} -N {dir} {source}; nanocode_try_status=$?; printf '%s' \"$nanocode_try_status\" > {done}; exit \"$nanocode_try_status\"",
            path = quote(&path),
            try_script = quote(&self.script.to_string_lossy()),
            dir = quote(&dir.to_string_lossy()),
            source = quote(&format!(". {}", quote(&script.to_string_lossy()))),
            done = quote(&dir.join("done").to_string_lossy()),
        );
        Ok((Sandbox { dir }, wrapped))
    }

    pub fn cleanup(&self) {
        drop(remove_tree(&self.root));
    }
}

pub fn remove_stale() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| {
                name.strip_prefix("nanocode-try-")
                    .or_else(|| name.strip_prefix("nanocode-review-"))
            })
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            drop(remove_tree(&entry.path()));
        }
    }
}

fn probe() -> bool {
    std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    pub fn finished(&self) -> bool {
        self.dir.join("done").exists()
    }

    fn upper(&self) -> PathBuf {
        self.dir.join("upperdir")
    }

    pub fn changes(&self) -> io::Result<Vec<Change>> {
        let mut changes = Vec::new();
        let upper = self.upper();
        if upper.is_dir() {
            walk(&upper, Path::new("/"), &self.dir, &mut changes)?;
        }
        Ok(changes)
    }

    pub fn commit(&self, changes: &[Change]) -> Vec<String> {
        let mut failures = Vec::new();
        for change in changes {
            if let Err(error) = apply(change) {
                failures.push(format!(
                    "{} ({}): {error}",
                    change.path.display(),
                    change.kind.label()
                ));
            }
        }
        failures
    }

    pub fn discard(self) {
        drop(remove_tree(&self.dir));
    }
}

fn walk(
    upper_root: &Path,
    local: &Path,
    sandbox: &Path,
    changes: &mut Vec<Change>,
) -> io::Result<()> {
    let upper = upper_root.join(local.strip_prefix("/").unwrap_or(local));
    let mut entries = std::fs::read_dir(&upper)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    entries.sort();
    for name in entries {
        let path = local.join(&name);
        if path.starts_with(sandbox) || EXCLUDED.iter().any(|excluded| path.starts_with(excluded)) {
            continue;
        }
        let changed = upper.join(&name);
        let metadata = std::fs::symlink_metadata(&changed)?;
        let file_type = metadata.file_type();
        let push = |kind, changes: &mut Vec<Change>| {
            changes.push(Change {
                kind,
                path: path.clone(),
                upper: changed.clone(),
            });
        };
        if file_type.is_symlink() {
            push(Kind::Symlink, changes);
        } else if file_type.is_dir() {
            let existing = std::fs::symlink_metadata(&path).ok();
            match existing {
                None => push(Kind::CreatedDir, changes),
                Some(_) if xattr(&changed, "user.overlay.opaque").as_deref() == Some(b"y") => {
                    push(Kind::ReplacedDir, changes);
                }
                Some(existing) if !existing.is_dir() => push(Kind::ReplacedDir, changes),
                Some(_) => {}
            }
            walk(upper_root, &path, sandbox, changes)?;
        } else if (file_type.is_char_device() && metadata.rdev() == 0)
            || (file_type.is_file() && xattr(&changed, "user.overlay.whiteout").is_some())
        {
            push(Kind::Deleted, changes);
        } else if file_type.is_file() || file_type.is_fifo() || file_type.is_socket() {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => push(Kind::Modified, changes),
                Err(_) => push(Kind::Added, changes),
            }
        }
    }
    Ok(())
}

fn xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let output = std::process::Command::new("getfattr")
        .args(["--absolute-names", "--only-values", "-h", "-n", name])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn apply(change: &Change) -> io::Result<()> {
    let local = &change.path;
    match change.kind {
        Kind::Symlink => {
            remove_any(local)?;
            std::os::unix::fs::symlink(std::fs::read_link(&change.upper)?, local)
        }
        Kind::ReplacedDir => {
            remove_any(local)?;
            std::fs::create_dir(local)
        }
        Kind::CreatedDir => match std::fs::create_dir(local) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            result => result,
        },
        Kind::Deleted => remove_any(local),
        Kind::Modified => {
            remove_any(local)?;
            move_file(&change.upper, local)
        }
        Kind::Added => move_file(&change.upper, local),
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
        result => result,
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    fn writable(path: &Path) {
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && metadata.is_dir()
        {
            drop(std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(0o700),
            ));
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.filter_map(std::result::Result::ok) {
                    writable(&entry.path());
                }
            }
        }
    }
    writable(path);
    std::fs::remove_dir_all(path)
}

pub fn unified_diff(old: &Path, new: &Path, label: &str) -> String {
    let output = std::process::Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--no-ext-diff", "--"])
        .arg(old)
        .arg(new)
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let old_name = old.to_string_lossy();
    let new_name = new.to_string_lossy();
    text.lines()
        .map(|line| {
            if line.starts_with("diff --git ") {
                format!("diff --git a/{label} b/{label}")
            } else if line.starts_with("--- ") && !line.ends_with("/dev/null") {
                format!("--- a/{label}")
            } else if line.starts_with("+++ ") && !line.ends_with("/dev/null") {
                format!("+++ b/{label}")
            } else if line.contains(old_name.trim_start_matches('/'))
                || line.contains(new_name.trim_start_matches('/'))
            {
                line.replace(old_name.trim_start_matches('/'), label)
                    .replace(new_name.trim_start_matches('/'), label)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
