use std::{
    env::current_dir,
    fs,
    io::{self, Error, ErrorKind, Read, Result, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
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

// We don't want to overwhelm the system with open files
const MAX_KIDS: usize = 512 + 256;

fn main() -> Result<()> {
    let mut dirs = Vec::with_capacity(512);
    let mut kids = Vec::with_capacity(MAX_KIDS);
    dirs.push(current_dir()?);
    let _stdout = io::stdout();
    let mut stdout = _stdout.lock();
    //. Loop over subdirectories, this is a replacement of recursion. (to prevent stack overflow and smashing)
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(fs::read_dir(&dir), dir) {
            let entry = try_continue!(entry, dir);
            let path = entry.path();
            let metadata = try_continue!(entry.metadata(), path);
            if metadata.is_dir() {
                if path.ends_with(".git") {
                    kids.push(try_continue!(git_gc(&path, &mut stdout), path));
                } else {
                    dirs.push(path);
                }
            } else if metadata.is_file() {
                if path.ends_with("Cargo.toml") {
                    kids.push(try_continue!(cargo_clean(&path, &mut stdout), path));
                } else if path.ends_with("Makefile") {
                    kids.push(try_continue!(make_clean(&path, &mut stdout), path));
                } else if path.ends_with("build.ninja") {
                    kids.push(try_continue!(ninja_clean(&path, &mut stdout), path));
                }
            } else if !metadata.is_symlink() {
                unreachable!();
            }

            if kids.len() == MAX_KIDS {
                kids = kids.into_iter().filter_map(ChildProcess::try_wait_log).collect();
                // If no sub-process finished wait for the earliest to finish
                if kids.len() == MAX_KIDS {
                    kids.remove(0).wait_log();
                }
            }
        }
    }
    writeln!(stdout, "Waiting for child processes to finish")?;
    // At the end wait for all currently running sub-processes to finish.
    kids.into_iter().for_each(ChildProcess::wait_log);
    writeln!(stdout, "Done")?;
    Ok(())
}

struct ChildProcess {
    child: Child,
    path: PathBuf,
}

impl ChildProcess {
    #[inline(always)]
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
    #[inline(always)]
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
    #[inline(always)]
    fn wait_log(mut self) {
        match self.child.wait() {
            Err(err) => log_err(&self.path, err),
            Ok(status) => self.log_output(status),
        }
    }
}

#[inline(always)]
fn make_clean(path: &Path, stdout: &mut impl Write) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    let path = path.parent().unwrap();
    writeln!(stdout, "make clean: {:?}", path)?;
    Ok(ChildProcess {
        child: Command::new("make")
            .arg("clean")
            .current_dir(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
        path: path.into(),
    })
}

#[inline(always)]
fn ninja_clean(path: &Path, stdout: &mut impl Write) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    let path = path.parent().unwrap();
    writeln!(stdout, "ninja clean: {:?}", path)?;
    Ok(ChildProcess {
        child: Command::new("ninja")
            .arg("clean")
            .current_dir(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
        path: path.into(),
    })
}

#[inline(always)]
fn cargo_clean(path: &Path, stdout: &mut impl Write) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    writeln!(stdout, "cargo clean: {:?}", path)?;
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
fn git_gc(path: &Path, stdout: &mut impl Write) -> Result<ChildProcess> {
    assert!(path.is_absolute());
    writeln!(stdout, "git gc: {:?}", path)?;
    Ok(ChildProcess {
        child: Command::new("git").arg("gc").current_dir(path).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?,
        path: path.into(),
    })
}
