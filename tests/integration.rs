use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Helper to create a directory and optionally a marker file inside it
fn create_project(base: &Path, subpath: &str, files: &[&str]) {
    let dir = base.join(subpath);
    fs::create_dir_all(&dir).unwrap();
    for file in files {
        File::create(dir.join(file)).unwrap();
    }
}

#[test]
fn test_directory_traversal_and_detection() {
    let temp = TempDir::new();
    let root = temp.path();

    // === Setup project structures ===

    // 1. Cargo project at root
    create_project(root, ".", &["Cargo.toml"]);

    // 2. Nested Makefile project
    create_project(root, "subdir/make_project", &["Makefile"]);

    // 3. Ninja project
    create_project(root, "subdir/ninja_project", &["build.ninja"]);

    // 4. Gradle project
    create_project(root, "android_app", &["gradlew"]);

    // 5. Git repo
    fs::create_dir_all(root.join("my_repo/.git")).unwrap();

    // 6. Node.js project with node_modules (this will actually be deleted)
    create_project(root, "web_app", &["package.json"]);
    let node_modules = root.join("web_app/node_modules");
    fs::create_dir_all(&node_modules).unwrap();
    File::create(node_modules.join("some_package.js")).unwrap();

    // 7. Nested node_modules
    create_project(root, "another_web/frontend", &["package.json"]);
    let nested_node_modules = root.join("another_web/frontend/node_modules");
    fs::create_dir_all(nested_node_modules.join("dep")).unwrap();
    File::create(nested_node_modules.join("dep/index.js")).unwrap();

    // === Edge cases ===

    // 8. Hidden directory should NOT be traversed (but .git is special)
    create_project(root, ".hidden_dir", &["Cargo.toml"]); // Should be skipped

    // 9. Project inside node_modules should be ignored (node_modules is in IGNORE_LIST)
    create_project(root, "ignored_path/node_modules/nested", &["Cargo.toml"]);

    // 10. Empty directories (no project files)
    fs::create_dir_all(root.join("empty_dir/nested_empty")).unwrap();

    // 11. Multiple project files in same directory
    create_project(root, "multi_project", &["Cargo.toml", "Makefile"]);

    // 12. Symlink to node_modules (should NOT be deleted - it's not a real dir)
    #[cfg(unix)]
    {
        let symlink_target = root.join("real_modules");
        fs::create_dir_all(&symlink_target).unwrap();
        // Create the project directory first, then add the symlink
        create_project(root, "symlink_project", &["package.json"]);
        std::os::unix::fs::symlink(&symlink_target, root.join("symlink_project/node_modules")).unwrap();
    }

    // 13. Deeply nested project
    create_project(root, "a/b/c/d/e/f", &["Cargo.toml"]);

    // === Run the tool ===

    let binary = env!("CARGO_BIN_EXE_code-clean");
    let output = Command::new(binary)
        .current_dir(root)
        .env("LOG", "1")
        .arg("-j")
        .arg("4") // Limit parallelism for predictable output
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("=== STDOUT ===\n{stdout}");
    println!("=== STDERR ===\n{stderr}");

    // Verify exit code
    assert!(output.status.success(), "Process should exit successfully");

    // === Verify results ===

    // 1. Cargo project at root - check cargo clean was logged
    assert!(stdout.contains("cargo") && stdout.contains("clean"), "1. Should log cargo clean command");

    // 2. Nested Makefile project - check make clean was logged
    assert!(stdout.contains("make") && stdout.contains("clean"), "2. Should log make clean command");

    // 3. Ninja project - check ninja clean was logged
    assert!(stdout.contains("ninja") && stdout.contains("clean"), "3. Should log ninja clean command");

    // 4. Gradle project - check gradlew clean was logged
    assert!(stdout.contains("gradlew") && stdout.contains("clean"), "4. Should log gradlew clean command");

    // 5. Git repo - check git gc was logged
    assert!(stdout.contains("git") && stdout.contains("gc"), "5. Should log git gc command");

    // 6. Node.js project - check node_modules was deleted
    assert!(!root.join("web_app/node_modules").exists(), "6. web_app/node_modules should be deleted");
    assert!(stdout.contains("rm -rf"), "6. Should log node_modules removal");

    // 7. Nested node_modules - check it was deleted
    assert!(
        !root.join("another_web/frontend/node_modules").exists(),
        "7. another_web/frontend/node_modules should be deleted"
    );

    // 8. Hidden directory should NOT be traversed
    assert!(!stdout.contains(".hidden_dir"), "8. Hidden directories should be skipped");

    // 9. Project inside node_modules should be ignored
    assert!(
        root.join("ignored_path/node_modules/nested/Cargo.toml").exists(),
        "9. Files inside node_modules should not be touched"
    );

    // 10. Empty directories - nothing to check, just shouldn't error

    // 11. Multiple project files in same directory - both commands should be logged
    let multi_cargo = stdout.contains("multi_project") && stdout.contains("cargo");
    let multi_make = stdout.contains("multi_project") && stdout.contains("make");
    assert!(multi_cargo && multi_make, "11. Should handle multiple project files in same directory");

    // 12. Symlink to node_modules (Unix only)
    #[cfg(unix)]
    {
        // The symlink should still exist (we don't delete symlinks)
        assert!(root.join("symlink_project/node_modules").exists(), "12. Symlink node_modules should still exist");
        assert!(
            fs::symlink_metadata(root.join("symlink_project/node_modules")).unwrap().file_type().is_symlink(),
            "12. Should still be a symlink"
        );
        // The symlink target directory should still exist
        assert!(root.join("real_modules").exists(), "12. Symlink target should not be deleted");
    }

    // 13. Deeply nested project - check it was found
    assert!(
        stdout.contains("a/b/c/d/e/f") || stdout.contains("a/b/c/d/e/f/Cargo.toml"),
        "13. Should find deeply nested projects"
    );

    // Verify "Waiting for child processes" message
    assert!(stdout.contains("Waiting for child processes"), "Should print waiting message");

    // Verify process completed successfully
    assert!(stdout.contains("Done"), "Should print Done at the end");
}

#[test]
fn test_jobs_argument_parsing() {
    let temp = TempDir::new();
    let root = temp.path();

    let binary = env!("CARGO_BIN_EXE_code-clean");

    // Test -j flag
    let output = Command::new(binary)
        .current_dir(root)
        .arg("-j")
        .arg("42")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Using 42 jobs"), "Should parse -j argument: {stdout}");

    // Test --jobs flag
    let output = Command::new(binary)
        .current_dir(root)
        .arg("--jobs")
        .arg("100")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Using 100 jobs"), "Should parse --jobs argument: {stdout}");

    // Test default (no -j flag) uses MAX_KIDS (768)
    let output =
        Command::new(binary).current_dir(root).stdout(Stdio::piped()).output().expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Using 768 jobs"), "Should use default MAX_KIDS: {stdout}");
}

#[test]
fn test_empty_directory() {
    let temp = TempDir::new();
    let root = temp.path();

    // Just an empty directory
    let binary = env!("CARGO_BIN_EXE_code-clean");
    let output = Command::new(binary)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    assert!(output.status.success(), "Should succeed on empty directory");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Done"), "Should complete successfully");
}

#[test]
fn test_node_modules_variations() {
    let temp = TempDir::new();
    let root = temp.path();

    // 1. node_modules without package.json (should NOT be deleted - no trigger)
    let orphan_nm = root.join("orphan/node_modules");
    fs::create_dir_all(&orphan_nm).unwrap();
    File::create(orphan_nm.join("file.js")).unwrap();

    // 2. package.json without node_modules (should not error)
    create_project(root, "no_modules", &["package.json"]);

    // 3. package.json with node_modules
    create_project(root, "with_modules", &["package.json"]);
    let nm = root.join("with_modules/node_modules");
    fs::create_dir_all(&nm).unwrap();
    File::create(nm.join("pkg.js")).unwrap();

    let binary = env!("CARGO_BIN_EXE_code-clean");
    let output = Command::new(binary)
        .current_dir(root)
        .env("LOG", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    assert!(output.status.success());

    // 1. Orphan node_modules should still exist (no package.json to trigger deletion)
    assert!(orphan_nm.exists(), "1. Orphan node_modules should NOT be deleted");

    // 2. package.json without node_modules - nothing to check, just shouldn't error

    // 3. node_modules with package.json should be deleted
    assert!(!nm.exists(), "3. node_modules with package.json should be deleted");
}

#[test]
fn test_log_environment_variable() {
    let temp = TempDir::new();
    let root = temp.path();

    // Create a simple project
    create_project(root, ".", &["Cargo.toml"]);

    let binary = env!("CARGO_BIN_EXE_code-clean");

    // Test LOG=1 shows commands
    let output = Command::new(binary)
        .current_dir(root)
        .env("LOG", "1")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cargo") && stdout.contains("clean"), "LOG=1 should show commands");

    // Test LOG=true also works
    let output = Command::new(binary)
        .current_dir(root)
        .env("LOG", "true")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cargo") && stdout.contains("clean"), "LOG=true should show commands");

    // Test without LOG env var - should NOT show commands
    let output = Command::new(binary)
        .current_dir(root)
        .env_remove("LOG")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("cargo clean"), "Without LOG should not show command details");
    assert!(stdout.contains("Using"), "Should still show jobs info");
    assert!(stdout.contains("Done"), "Should still show Done");

    // Test LOG=0 - should NOT show commands
    let output = Command::new(binary)
        .current_dir(root)
        .env("LOG", "0")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run code-clean");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("cargo clean"), "LOG=0 should not show command details");
}

/// A simple temporary directory guard that removes the directory on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        use std::time::SystemTime;
        let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let tempdir = std::env::temp_dir();
        let pid = std::process::id();
        loop {
            let ctr = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir_name = format!("code_clean_test_{pid}_{ts}_{ctr}");
            let path = tempdir.join(dir_name);
            if fs::create_dir(&path).is_ok() {
                return Self(path);
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.0) {
            eprintln!("Failed to remove temp dir {:?}: {}", self.0, e);
        }
    }
}
