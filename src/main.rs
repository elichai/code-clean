use std::ffi::OsStr;
use std::process::ChildStderr;
use std::{
    env::current_dir,
    fs,
    io::{self, Error, ErrorKind, Read, Result, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
};

macro_rules! try_continue {
    ($stderr_manager:expr, $expr:expr, $path:ident) => {
        match $expr {
            Ok(val) => val,
            Err(err) => {
                $stderr_manager.log_err(&$path, err)?;
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
    stderr: StdErrManager,
}

impl ChildrenManager {
    #[inline(always)]
    fn new(cap: usize) -> Self {
        Self { kids: Vec::with_capacity(cap), stdout: io::stdout().lock(), stderr: StdErrManager::new() }
    }
    #[inline(always)]
    fn push_wait(&mut self, kid: ChildProcess) -> Result<()> {
        if self.kids.len() == self.kids.capacity() {
            let mut res = Ok(());
            self.kids.retain_mut(|child| {
                if res.is_err() {
                    return true;
                }
                match child.try_wait_log(&mut self.stderr) {
                    Ok(b) => b,
                    Err(e) => {
                        res = Err(e);
                        true
                    }
                }
            });
            res?;
            // If no sub-process finished wait for the earliest to finish
            if self.kids.len() == self.kids.capacity() {
                self.kids.remove(0).wait_log(&mut self.stderr)?;
            }
        }
        self.kids.push(kid);
        Ok(())
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
            self.push_wait(child)?
        }
        Ok(())
    }
}

impl Drop for ChildrenManager {
    #[inline(always)]
    fn drop(&mut self) {
        // Wait on all sub-processes.
        self.kids.drain(..).for_each(|child| {
            child.wait_log(&mut self.stderr).expect("Failed to wait on child process while dropping ChildrenManager")
        });
    }
}

struct StdErrManager {
    stderr: io::StderrLock<'static>,
    buf: String,
}

impl StdErrManager {
    #[inline(always)]
    fn new() -> Self {
        Self { stderr: io::stderr().lock(), buf: String::with_capacity(256) }
    }

    #[inline(always)]
    fn log_err(&mut self, path: &impl AsRef<Path>, err: impl std::error::Error) -> Result<()> {
        writeln!(&mut self.stderr, "Error in: {:?} => {}", path.as_ref(), err)
    }

    #[inline(always)]
    fn log_child_stderr(
        &mut self,
        path: &impl AsRef<Path>,
        status: ExitStatus,
        child_stderr: &mut Option<ChildStderr>,
    ) -> Result<()> {
        self.buf.clear();

        if let Some(stderr_handler) = child_stderr {
            if let Err(err) = stderr_handler.read_to_string(&mut self.buf) {
                self.log_err(path, err)?;
            }
        }
        self.log_err(path, Error::new(ErrorKind::Other, format!("exit status: {status}, stderr: {}", self.buf)))
    }
}

fn main() -> Result<()> {
    let mut dirs = Vec::with_capacity(512);
    let mut kids_manager = ChildrenManager::new(MAX_KIDS);
    dirs.push(current_dir()?);
    //. Loop over subdirectories, this is a replacement of recursion. (to prevent stack overflow and smashing)
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(&mut kids_manager.stderr, fs::read_dir(&dir), dir) {
            let entry = try_continue!(&mut kids_manager.stderr, entry, dir);
            let path = entry.path();
            let metadata = try_continue!(&mut kids_manager.stderr, entry.metadata(), path);
            try_continue!(&mut kids_manager.stderr, kids_manager.handle_path(&path), path);
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
    #[inline(always)]
    fn new_make_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("make", &["clean".as_ref()], path, stdout)
    }
    #[inline(always)]
    fn new_gradlew_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("./gradlew", &["clean".as_ref()], path, stdout)
    }
    #[inline(always)]
    fn new_ninja_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("ninja", &["clean".as_ref()], path, stdout)
    }
    #[inline(always)]
    fn new_cargo_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("cargo", &["clean".as_ref(), "--manifest-path".as_ref(), path.as_ref()], path, stdout)
    }
    #[inline(always)]
    fn new_git_clean(path: &Path, stdout: &mut io::StdoutLock<'_>) -> Result<Self> {
        Self::new("git", &["gc".as_ref()], path, stdout)
    }

    #[inline(always)]
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
    fn try_wait_log(&mut self, stderr_manager: &mut StdErrManager) -> Result<bool> {
        match self.child.try_wait() {
            Err(err) => stderr_manager.log_err(&self.path, err).map(|()| false),
            Ok(None) => Ok(true),
            Ok(Some(status)) => self.log_output(status, stderr_manager).map(|()| false),
        }
    }
    #[inline(always)]
    fn log_output(&mut self, status: ExitStatus, stderr_manager: &mut StdErrManager) -> Result<()> {
        if !status.success() {
            stderr_manager.log_child_stderr(&self.path, status, &mut self.child.stderr)
        } else {
            Ok(())
        }
    }
    #[inline(always)]
    fn wait_log(mut self, stderr_manager: &mut StdErrManager) -> Result<()> {
        match self.child.wait() {
            Err(err) => stderr_manager.log_err(&self.path, err),
            Ok(status) => self.log_output(status, stderr_manager),
        }
    }
}
