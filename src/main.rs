use std::ffi::OsStr;
use std::process::ChildStderr;
use std::{
    env::{self, current_dir},
    fs,
    io::{self, Error, ErrorKind, Read, Result, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    str::FromStr,
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
            self.try_wait_remove()?;
        }
        // If no sub-process finished wait for the earliest to finish
        if self.kids.len() == self.kids.capacity() {
            self.wait_remove()?;
        }

        self.kids.push(kid);
        Ok(())
    }

    #[inline(always)]
    fn try_wait_remove(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.kids.len() {
            if self.kids[i].try_wait_log(&mut self.stderr)? {
                self.kids.swap_remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    #[inline(always)]
    fn wait_remove(&mut self) -> Result<()> {
        match os_wait::wait_on_children(&self.kids) {
            Err(err) => self.stderr.log_os_err(err),
            Ok((status, idx)) => self.kids.swap_remove(idx).log_output(status, &mut self.stderr),
        }
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
    fn log_os_err(&mut self, err: impl std::error::Error) -> Result<()> {
        writeln!(&mut self.stderr, "Operating System Error: {err}")
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
    let kids_limit = env::args()
        .position(|a| a == "-j" || a == "--jobs")
        .and_then(|pos| env::args().nth(pos + 1).map(|v| usize::from_str(&v).unwrap()))
        .unwrap_or(MAX_KIDS);
    println!("Using {kids_limit} jobs");
    let mut dirs = Vec::with_capacity(512);
    let mut kids_manager = ChildrenManager::new(kids_limit);
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
                .register_child()
                .spawn()?,
            path: path.into(),
        })
    }

    #[inline(always)]
    fn try_wait_log(&mut self, stderr_manager: &mut StdErrManager) -> Result<bool> {
        match self.child.try_wait().transpose() {
            None => Ok(true),
            Some(res) => self.log_res(stderr_manager, res).map(|()| false),
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
    fn log_res(&mut self, stderr_manager: &mut StdErrManager, res: Result<ExitStatus>) -> Result<()> {
        match res {
            Err(err) => stderr_manager.log_err(&self.path, err),
            Ok(status) => self.log_output(status, stderr_manager),
        }
    }
    #[inline(always)]
    fn wait_log(mut self, stderr_manager: &mut StdErrManager) -> Result<()> {
        let res = self.child.wait();
        self.log_res(stderr_manager, res)
    }
}

trait RegisterChild {
    fn register_child(&mut self) -> &mut Self;
}

#[cfg(unix)]
mod os_wait {
    use crate::{ChildProcess, RegisterChild};
    use std::ffi::c_int;
    use std::io::Result;
    use std::os::unix::prelude::ExitStatusExt;
    use std::os::unix::process::CommandExt;
    use std::process::{abort, Command, ExitStatus};
    use std::sync::Once;
    #[allow(non_camel_case_types)]
    type pid_t = i32;
    extern "C" {
        fn waitpid(pid: pid_t, wstatus: *mut c_int, options: c_int) -> pid_t;
        fn getpgrp() -> pid_t;
    }
    fn get_pgid() -> pid_t {
        static mut PGID: pid_t = 0;
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            PGID = getpgrp();
            if PGID == -1 {
                eprintln!("{:?}", std::io::Error::last_os_error());
                abort();
            }
        });
        unsafe { PGID }
    }

    impl RegisterChild for Command {
        #[inline(always)]
        fn register_child(&mut self) -> &mut Self {
            self.process_group(get_pgid())
        }
    }

    /// Returns the exit status and the index of the child process that exited.
    #[inline(always)]
    pub(super) fn wait_on_children(processes: &[ChildProcess]) -> Result<(ExitStatus, usize)> {
        let mut status: c_int = 0;
        let pid = match unsafe { waitpid(-get_pgid(), &mut status, 0) } {
            -1 => return Err(std::io::Error::last_os_error()),
            pid if pid.is_positive() => pid,
            _ => abort(),
        };
        let index = processes.iter().position(|p| p.child.id() == pid as u32).unwrap();
        Ok((ExitStatus::from_raw(status), index))
    }
}

#[cfg(windows)]
mod os_wait {
    use crate::{ChildProcess, RegisterChild};
    use std::ffi::{c_int, c_ulong};
    use std::io::Result;
    use std::os::windows::{io::AsRawHandle, process::ExitStatusExt, raw::HANDLE};
    use std::process::{Command, ExitStatus};
    use std::{cmp, ptr};

    impl RegisterChild for Command {
        #[inline(always)]
        fn register_child(&mut self) -> &mut Self {
            self
        }
    }

    type DWORD = c_ulong;
    type BOOL = c_int;
    type LPDWORD = *mut DWORD;

    const MAXIMUM_WAIT_OBJECTS: usize = 64;
    const WAIT_OBJECT_0: DWORD = 0;
    const WAIT_FAILED: DWORD = 0xFFFFFFFF;
    const INFINITE: DWORD = 0xFFFFFFFF;
    const FALSE: BOOL = 0;
    extern "system" {
        fn WaitForMultipleObjects(
            n_count: DWORD,
            lp_handles: *const HANDLE,
            b_wait_all: BOOL,
            dw_milliseconds: DWORD,
        ) -> DWORD;
        fn GetExitCodeProcess(h_process: HANDLE, lp_exit_code: LPDWORD) -> BOOL;
    }

    /// Returns the exit status and the index of the child process that exited.
    #[inline(always)]
    pub(super) fn wait_on_children(processes: &[ChildProcess]) -> Result<(ExitStatus, usize)> {
        // Sadly windows doesn't support waiting on more than 64 processes at once.
        let mut handles = [ptr::null_mut(); MAXIMUM_WAIT_OBJECTS];
        let size = cmp::min(processes.len(), MAXIMUM_WAIT_OBJECTS);
        for (i, p) in processes.iter().take(size).enumerate() {
            handles[i] = p.child.as_raw_handle();
        }
        let index = match unsafe { WaitForMultipleObjects(size as DWORD, handles.as_ptr(), FALSE, INFINITE) } {
            WAIT_FAILED => return Err(std::io::Error::last_os_error()),
            ret => (ret - WAIT_OBJECT_0) as usize,
        };
        let mut status = 0;
        let handle = processes[index].child.as_raw_handle();
        if unsafe { GetExitCodeProcess(handle, &mut status) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((ExitStatus::from_raw(status), index))
    }
}
