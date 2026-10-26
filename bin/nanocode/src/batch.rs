use std::{
    io,
    path::{Path, PathBuf},
};

use crate::sandbox::{remove_any, remove_tree};

pub enum Original {
    Absent,
    File(PathBuf),
    Dir(PathBuf),
    Symlink(PathBuf),
}

pub struct Recorded {
    pub path: PathBuf,
    pub original: Original,
}

impl Recorded {
    pub fn discard(&self) {
        match &self.original {
            Original::File(copy) => drop(std::fs::remove_file(copy)),
            Original::Dir(copy) => drop(remove_tree(copy)),
            Original::Absent | Original::Symlink(_) => {}
        }
    }

    pub fn restore(&self) -> io::Result<()> {
        let path = &self.path;
        match &self.original {
            Original::Absent => remove_any(path),
            Original::File(copy) => {
                remove_any(path)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(copy, path).map(drop)
            }
            Original::Dir(copy) => {
                remove_any(path)?;
                copy_tree(copy, path)
            }
            Original::Symlink(target) => {
                remove_any(path)?;
                std::os::unix::fs::symlink(target, path)
            }
        }
    }

    pub fn status(&self) -> &'static str {
        let exists = std::fs::symlink_metadata(&self.path).is_ok();
        match (&self.original, exists) {
            (Original::Absent, true) => "A",
            (Original::Absent, false) => "·",
            (_, false) => "D",
            (_, true) => "M",
        }
    }

    pub fn before(&self) -> PathBuf {
        match &self.original {
            Original::File(copy) | Original::Dir(copy) => copy.clone(),
            Original::Absent if self.path.is_dir() => empty_dir(),
            Original::Absent | Original::Symlink(_) => PathBuf::from("/dev/null"),
        }
    }

    pub fn after(&self) -> PathBuf {
        match std::fs::symlink_metadata(&self.path) {
            Ok(_) => self.path.clone(),
            Err(_) if matches!(self.original, Original::Dir(_)) => empty_dir(),
            Err(_) => PathBuf::from("/dev/null"),
        }
    }
}

fn empty_dir() -> PathBuf {
    let path = std::env::temp_dir()
        .join(format!("nanocode-review-{}", std::process::id()))
        .join("empty");
    drop(std::fs::create_dir_all(&path));
    path
}

pub fn snapshot(path: &Path, store: &Path) -> io::Result<Original> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Original::Absent),
        Err(error) => return Err(error),
    };
    let copy = store.join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir_all(store)?;
    if metadata.is_symlink() {
        Ok(Original::Symlink(std::fs::read_link(path)?))
    } else if metadata.is_dir() {
        copy_tree(path, &copy)?;
        Ok(Original::Dir(copy))
    } else {
        std::fs::copy(path, &copy)?;
        Ok(Original::File(copy))
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cp -a {} {} failed with {status}",
            from.display(),
            to.display()
        )))
    }
}
