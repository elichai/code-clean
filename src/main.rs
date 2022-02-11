use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::{
    env::current_dir,
    fs,
    io::{Error, ErrorKind, Result},
    path::Path,
    process::Command,
};

#[inline(always)]
fn log_err(path: &impl AsRef<Path>, err: impl std::error::Error) {
    eprintln!("Error in: {:?} => {}", path.as_ref(), err);
}

macro_rules! try_continue {
    ($expr:expr, $path:ident) => {
        match $expr {
            Ok(val) => val,
            Err(err) => {
                log_err(&$path, err);
                continue;
            }
        }
    };
}

const MAX_KIDS: usize = 8192;

fn main() -> Result<()> {
    let mut dirs = Vec::with_capacity(512);
    let mut kids = Vec::with_capacity(MAX_KIDS);
    dirs.push(current_dir()?);
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(fs::read_dir(&dir), dir) {
            let entry = try_continue!(entry, dir);
            let path = entry.path();
            let metadata = try_continue!(entry.metadata(), path);
            if metadata.is_dir() {
                if path.ends_with(".git") {
                    kids.push(try_continue!(git_gc(&path), path));
                } else {
                    dirs.push(path);
                }
            } else if metadata.is_file() {
                if path.ends_with("Cargo.toml") {
                    kids.push(try_continue!(clean_cargo(&path), path));
                }
            } else if !metadata.is_symlink() {
                unreachable!();
            }

            if kids.len() == MAX_KIDS {
                kids = kids.into_iter().filter_map(ChildProcess::try_wait_log).collect();
                // If none finished, then wait for finish
                if kids.len() == MAX_KIDS {
                    kids.pop().unwrap().wait_log();
                }
            }
        }
    }
    kids.into_iter().for_each(ChildProcess::wait_log);
    println!("Done");
    Ok(())
}

struct ChildProcess {
    child: Child,
    path: PathBuf,
}

impl ChildProcess {
    fn try_wait_log(mut self) -> Option<Self> {
        match self.child.try_wait() {
            Err(err) => {
                log_err(&self.path, err);
                None
            }
            Ok(None) => Some(self),
            Ok(Some(status)) => {
                self.log_output(status);
                None
            }
        }
    }
    fn log_output(self, status: ExitStatus) {
        if !status.success() {
            let mut stderr = String::new();
            if let Some(mut stderr_handler) = self.child.stderr {
                if let Err(err) = stderr_handler.read_to_string(&mut stderr) {
                    log_err(&self.path, err);
                }
            }
            log_err(&self.path, Error::new(ErrorKind::Other, format!("exit status: {status}, stderr: {stderr}")));
        }
    }
    fn wait_log(mut self) {
        match self.child.wait() {
            Err(err) => log_err(&self.path, err),
            Ok(status) => self.log_output(status),
        }
    }
}

#[inline(always)]
fn clean_cargo(path: &Path) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    println!("cargo clean: {:?}", path);
    Ok(ChildProcess {
        child: Command::new("cargo")
            .args(["clean", "--manifest-path"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
        path: path.into(),
    })
}

#[inline(always)]
fn git_gc(path: &Path) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    println!("git gc: {:?}", path);
    Ok(ChildProcess {
        child: Command::new("git").arg("gc").current_dir(path).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?,
        path: path.into(),
    })
}
