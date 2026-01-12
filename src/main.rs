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

#[inline(always)]
fn is_hidden(path: &Path) -> bool {
    path.file_name().and_then(OsStr::to_str).map(|s| s.starts_with('.')).unwrap_or(false)
}

struct ChildrenManager {
    kids: Vec<ChildProcess>,
    max_kids: usize,
    stdout: io::StdoutLock<'static>,
    stderr: StdErrManager,
    log_command: bool,
}

impl ChildrenManager {
    #[inline(always)]
    fn new(cap: usize, log_command: bool) -> Self {
        Self {
            kids: Vec::with_capacity(cap),
            max_kids: cap,
            stdout: io::stdout().lock(),
            stderr: StdErrManager::new(),
            log_command,
        }
    }
    #[inline(always)]
    fn push_wait(&mut self, kid: ChildProcess) -> Result<()> {
        // IMPORTANT: Add the child FIRST, before calling wait_remove.
        // Otherwise, waitpid could return this child's PID before we track it.
        self.kids.push(kid);

        // Now enforce the limit
        if self.kids.len() >= self.max_kids {
            self.try_wait_remove()?;
        }
        // If no sub-processes have exited yet, we have to wait for one to exit.
        if self.kids.len() >= self.max_kids {
            self.wait_remove()?;
        }
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
                "Cargo.toml" => Some(self.new_child_cargo_clean(path)),
                "Makefile" => Some(self.new_child_make_clean(path)),
                "build.ninja" => Some(self.new_child_ninja_clean(path)),
                "gradlew" => Some(self.new_child_gradlew_clean(path)),
                ".git" => Some(self.new_child_git_clean(path)),
                "package.json" => {
                    self.new_child_node_modules(&path.with_file_name("node_modules")).map(|_| None).transpose()
                }
                _ => None,
            })
            .transpose()?;
        if let Some(child) = child {
            self.push_wait(child)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn print_command(&mut self, program: &str, args: &[&OsStr], path: &Path) -> Result<()> {
        if !self.log_command {
            return Ok(());
        }
        write!(&mut self.stdout, "[{path}]: {program} ", path = path.display())?;
        for arg in args.iter().map(|s| s.to_str().expect("Expect valid utf-8")) {
            write!(&mut self.stdout, "{arg}")?;
        }
        writeln!(&mut self.stdout)
    }

    #[inline(always)]
    fn new_child_node_modules(&mut self, path: &Path) -> Result<()> {
        assert!(path.ends_with("node_modules"));
        // use symlink_metadata to make sure it's a directory and not follow the symlink
        if !path.exists() || !fs::symlink_metadata(path)?.is_dir() {
            return Ok(());
        }
        if self.log_command {
            writeln!(&mut self.stdout, "[{path}]: rm -rf ", path = path.display())?;
        }
        fs::remove_dir_all(path)
    }

    #[inline(always)]
    fn new_child(&mut self, program: &str, args: &[&OsStr], path: &Path) -> Result<ChildProcess> {
        self.print_command(program, args, path)?;
        ChildProcess::new(program, args, path)
    }
    #[inline(always)]
    fn new_child_make_clean(&mut self, path: &Path) -> Result<ChildProcess> {
        self.new_child("make", &["clean".as_ref()], path)
    }
    #[inline(always)]
    fn new_child_gradlew_clean(&mut self, path: &Path) -> Result<ChildProcess> {
        self.new_child("./gradlew", &["clean".as_ref()], path)
    }
    #[inline(always)]
    fn new_child_ninja_clean(&mut self, path: &Path) -> Result<ChildProcess> {
        self.new_child("ninja", &["clean".as_ref()], path)
    }
    #[inline(always)]
    fn new_child_cargo_clean(&mut self, path: &Path) -> Result<ChildProcess> {
        self.new_child("cargo", &["clean".as_ref(), "--manifest-path".as_ref(), path.as_ref()], path)
    }
    #[inline(always)]
    fn new_child_git_clean(&mut self, path: &Path) -> Result<ChildProcess> {
        self.new_child("git", &["gc".as_ref()], path)
    }
}

impl Drop for ChildrenManager {
    #[inline(always)]
    fn drop(&mut self) {
        // Wait on all sub-processes.
        self.kids.drain(..).for_each(|child| {
            child.wait_log(&mut self.stderr).expect("Failed to wait on child process while dropping ChildrenManager");
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
        const IGNORE_LIST: &[&str] = &["No rule to make target"];

        self.buf.clear();
        if let Some(stderr_handler) = child_stderr {
            if let Err(err) = stderr_handler.read_to_string(&mut self.buf) {
                return self.log_err(path, err);
            }
        }
        if IGNORE_LIST.iter().any(|&s| self.buf.contains(s)) {
            return Ok(()); // Ignore this error
        }
        self.log_err(path, Error::new(ErrorKind::Other, format!("{status}, stderr: {}", self.buf)))
    }
}

fn main() -> Result<()> {
    let kids_limit = env::args()
        .position(|a| a == "-j" || a == "--jobs")
        .and_then(|pos| env::args().nth(pos + 1).map(|v| usize::from_str(&v).unwrap()))
        .unwrap_or(MAX_KIDS);
    println!("Using {kids_limit} jobs");
    let is_log_out = env::var("LOG").map(|v| v == "1" || v == "true").unwrap_or(false);
    let mut dirs = Vec::with_capacity(512);
    let mut kids_manager = ChildrenManager::new(kids_limit, is_log_out);
    dirs.push(current_dir()?);
    //. Loop over subdirectories, this is a replacement of recursion. (to prevent stack overflow and smashing)
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(&mut kids_manager.stderr, fs::read_dir(&dir), dir) {
            let entry = try_continue!(&mut kids_manager.stderr, entry, dir);
            let path = entry.path();
            let metadata = try_continue!(&mut kids_manager.stderr, entry.metadata(), path);
            try_continue!(&mut kids_manager.stderr, kids_manager.handle_path(&path), path);
            // This won't traverse symlinks, as `entry.metadata()` is the same as `symlink_metadata()`.
            if metadata.is_dir() && !should_ignore(&path) && !is_hidden(&path) {
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
    fn new(program: &str, args: &[&OsStr], path: &Path) -> Result<Self> {
        assert!(path.is_absolute());
        let path = path.parent().unwrap();
        Ok(Self {
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
        self.child
            .try_wait()
            .transpose()
            .map_or_else(|| Ok(false), |res| self.log_res(stderr_manager, res).map(|()| true))
    }
    #[inline(always)]
    fn log_output(&mut self, status: ExitStatus, stderr_manager: &mut StdErrManager) -> Result<()> {
        if status.success() {
            Ok(())
        } else {
            stderr_manager.log_child_stderr(&self.path, status, &mut self.child.stderr)
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
    use std::sync::atomic::{AtomicI32, Ordering};
    #[allow(non_camel_case_types)]
    type pid_t = i32;
    extern "C" {
        fn waitpid(pid: pid_t, wstatus: *mut c_int, options: c_int) -> pid_t;
        fn getpgrp() -> pid_t;
    }
    fn get_pgid() -> pid_t {
        static PGID: AtomicI32 = AtomicI32::new(-1);
        let cur_pgid = PGID.load(Ordering::Relaxed);
        if cur_pgid != -1 {
            // Check if we already have a pgid
            return cur_pgid;
        }
        // We don't have a pgid yet, so we need to get one.
        let pgid = unsafe { getpgrp() };
        // getpgid(), and the BSD-specific getpgrp() return a process group on success.
        // On error, -1 is returned, and errno is set to indicate the error.
        if pgid == -1 {
            eprintln!("{:?}", std::io::Error::last_os_error());
            abort();
        }
        let last_pgid = PGID.swap(pgid, Ordering::Relaxed);
        // Make sure that if we raced another thread we got the same pgid.
        assert!(last_pgid == -1 || last_pgid == pgid, "last_pgid: {last_pgid}, pgid: {pgid}");
        pgid
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
        let pid_u32 = u32::try_from(pid).expect("pid should fit in u32");
        let index = processes.iter().position(|p| p.child.id() == pid_u32).unwrap_or_else(|| {
            let pids = processes.iter().map(|p| p.child.id()).collect::<Vec<_>>();
            panic!("waitpid returned unknown pid: {pid_u32}, known pids: {pids:?}")
        });
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
        // If the function succeeds, the return value is nonzero.
        // If the function fails, the return value is zero.
        if unsafe { GetExitCodeProcess(handle, &mut status) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((ExitStatus::from_raw(status), index))
    }
}
