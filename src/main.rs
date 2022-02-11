use std::{
    env::current_dir,
    fs,
    io::{Error, ErrorKind, Result},
    path::Path,
    process::Command,
    str,
};

macro_rules! try_continue {
    ($expr:expr, $path:ident) => {
        match $expr {
            Ok(val) => val,
            Err(err) => {
                eprintln!("Error in: {:?} => {}", $path, err);
                continue;
            }
        }
    };
}

fn main() -> Result<()> {
    let mut dirs = Vec::with_capacity(512);
    dirs.push(current_dir()?);
    while let Some(dir) = dirs.pop() {
        for entry in try_continue!(fs::read_dir(&dir), dir) {
            let entry = try_continue!(entry, dir);
            let path = entry.path();
            let metadata = try_continue!(entry.metadata(), path);
            if metadata.is_dir() {
                if path.ends_with(".git") {
                    try_continue!(git_gc(&path), path);
                } else {
                    dirs.push(path);
                }
            } else if metadata.is_file() {
                if path.ends_with("Cargo.toml") {
                    try_continue!(clean_cargo(&path), path);
                }
            } else if !metadata.is_symlink() {
                unreachable!();
            }
        }
    }
    println!("Done");
    Ok(())
}

#[inline(always)]
fn clean_cargo(path: &Path) -> Result<()> {
    assert!(path.is_absolute());
    println!("cargo clean: {:?}", path);
    exec_cmd(Command::new("cargo").args(["clean", "--manifest-path"]).arg(path))
}

#[inline(always)]
fn git_gc(path: &Path) -> Result<()> {
    assert!(path.is_absolute());
    println!("git gc: {:?}", path);
    exec_cmd(Command::new("git").arg("gc").current_dir(path))
}

#[inline(always)]
fn exec_cmd(cmd: &mut Command) -> Result<()> {
    let output = cmd.output()?;
    let status = output.status;
    if !output.status.success() {
        let stderr = str::from_utf8(&output.stderr).unwrap_or("");
        return Err(Error::new(
            ErrorKind::Other,
            format!("Command: {cmd:?} returned error: {status}, stderr: {stderr}"),
        ));
    }
    Ok(())
}
