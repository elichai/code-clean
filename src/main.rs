use std::ffi::OsStr;
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

#[inline(always)]
fn should_ignore(path: &Path) -> bool {
    const IGNORE_LIST: &[&str] = &["node_modules"];
    IGNORE_LIST.iter().any(|&ignore| path.ends_with(ignore))
}

struct ChildrenManager {
    kids: Vec<ChildProcess>,
    stdout: io::StdoutLock<'static>,
}

impl ChildrenManager {
    #[inline(always)]
    fn new(cap: usize) -> Self {
        Self { kids: Vec::with_capacity(cap), stdout: io::stdout().lock() }
    }
    #[inline(always)]
    fn push_wait(&mut self, kid: ChildProcess) {
        if self.kids.len() == self.kids.capacity() {
            self.kids.retain_mut(ChildProcess::try_wait_log);
            // If no sub-process finished wait for the earliest to finish
            if self.kids.len() == self.kids.capacity() {
                self.kids.remove(0).wait_log();
            }
        }
        self.kids.push(kid);
    }
    #[inline(always)]
    fn handle_path(&mut self, path: &Path) -> Result<()> {
        let child = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|file_name| match file_name {
                "Cargo.toml" => Some(ChildProcess::new_cargo_clean(path, &mut self.stdout)),
                "Makefile" => Some(ChildProcess::new_make_clean(path, &mut self.stdout)),
                "build.ninja" => Some(ChildProcess::new_ninja_clean(path, &mut self.stdout)),
                "gradlew" => Some(ChildProcess::new_gradlew_clean(path, &mut self.stdout)),
                ".git" => Some(ChildProcess::new_git_clean(path, &mut self.stdout)),
                _ => None,
            })
            .transpose()?;
        if let Some(child) = child {
            self.push_wait(child)
        }
        Ok(())
    }
}

impl Drop for ChildrenManager {
    fn drop(&mut self) {
        // Wait on all sub-processes.
        self.kids.drain(..).for_each(ChildProcess::wait_log);
    }
}

fn main() -> Result<()> {
    let mut dirs = Vec::with_capacity(512);
    let mut kids_manager = ChildrenManager::new(MAX_KIDS);
    dirs.push(current_dir()?);
    //. Loop over subdirectories, this is a replacement of recursion. (to prevent stack overflow and smashing)
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(fs::read_dir(&dir), dir) {
            let entry = try_continue!(entry, dir);
            let path = entry.path();
            let metadata = try_continue!(entry.metadata(), path);
            try_continue!(kids_manager.handle_path(&path), path);
            if metadata.is_dir() && !should_ignore(&path) {
                dirs.push(path);
            }
        }
    }
    writeln!(kids_manager.stdout, "Waiting for child processes to finish")?;
    // At the end wait for all currently running sub-processes to finish.
    drop(kids_manager);
    println!("Done");
    Ok(())
}

struct ChildProcess {
    child: Child,
    path: PathBuf,
}

impl ChildProcess {
    fn new_make_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("make", &["clean".as_ref()], path, stdout)
    }
    fn new_gradlew_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("./gradlew", &["clean".as_ref()], path, stdout)
    }
    fn new_ninja_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("ninja", &["clean".as_ref()], path, stdout)
    }
    fn new_cargo_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("cargo", &["clean".as_ref(), "--manifest-path".as_ref(), path.as_ref()], path, stdout)
    }
    fn new_git_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("git", &["gc".as_ref()], path, stdout)
    }

    fn new(program: &str, args: &[&OsStr], path: &Path, stdout: &mut impl Write) -> Result<Self> {
        assert!(path.is_absolute());
        let path = path.parent().unwrap();
        writeln!(stdout, "{program} {args:?}: {path:?}")?;
        Ok(ChildProcess {
            child: Command::new(program)
                .args(args)
                .current_dir(path)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
            path: path.into(),
        })
    }

    #[inline(always)]
    fn try_wait_log(&mut self) -> bool {
        match self.child.try_wait() {
            Err(err) => {
                log_err(&self.path, err);
                false
            }
            Ok(None) => true,
            Ok(Some(status)) => {
                self.log_output(status);
                false
            }
        }
    }
    #[inline(always)]
    fn log_output(&mut self, status: ExitStatus) {
        if !status.success() {
            let mut stderr = String::new();
            if let Some(stderr_handler) = &mut self.child.stderr {
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
