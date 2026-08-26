use rtk::{
    execute_with_options, BeforeStartError, CancellationToken, ExecuteError, ExecuteOptions,
    ExecutionRoute, MayHaveStartedError, MayHaveStartedKind,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[test]
fn filtered_execution_preserves_cwd_and_exit_status() {
    let temp = tempfile::tempdir().expect("temporary directory");
    fs::write(temp.path().join("input.txt"), "one\ntwo\nthree\n").expect("write fixture");
    let options = ExecuteOptions::default().with_cwd(temp.path());

    let filtered = execute_with_options("wc -l input.txt", &options).expect("execute command");
    assert_eq!(filtered.stdout.trim(), "3");
    assert_eq!(filtered.exit_code, 0);
    assert_eq!(filtered.route, ExecutionRoute::Filtered { tool: "wc" });

    #[cfg(windows)]
    let cwd_command = "cd";
    #[cfg(not(windows))]
    let cwd_command = "pwd";
    let cwd = execute_with_options(cwd_command, &options).expect("execute cwd command");
    assert_eq!(
        Path::new(cwd.stdout.trim())
            .canonicalize()
            .expect("output cwd"),
        temp.path().canonicalize().expect("expected cwd")
    );

    #[cfg(windows)]
    let failing_command = "exit /B 23";
    #[cfg(not(windows))]
    let failing_command = "exit 23";
    let failed = execute_with_options(failing_command, &options).expect("execute failing command");
    assert_eq!(failed.exit_code, 23);
}

#[test]
fn cancellation_before_spawn_is_retry_safe() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let options = ExecuteOptions::default().with_cancellation(cancellation);

    let error = execute_with_options("wc -l missing", &options).expect_err("cancel execution");

    assert!(matches!(
        error,
        ExecuteError::BeforeStart(BeforeStartError::Cancelled)
    ));
}

#[test]
fn zero_timeout_is_retry_safe() {
    let options = ExecuteOptions::default().with_timeout(Duration::ZERO);

    let error = execute_with_options("wc -l missing", &options).expect_err("timeout execution");

    assert!(matches!(
        error,
        ExecuteError::BeforeStart(BeforeStartError::TimedOut)
    ));
}

#[test]
fn cancellation_after_spawn_is_never_retry_safe() {
    let cancellation = CancellationToken::new();
    let options = ExecuteOptions::default().with_cancellation(cancellation.clone());
    #[cfg(windows)]
    let command = "echo started & ping -n 6 127.0.0.1 >NUL";
    #[cfg(not(windows))]
    let command = "printf started; sleep 5";

    let worker = std::thread::spawn(move || execute_with_options(command, &options));
    std::thread::sleep(Duration::from_millis(50));
    cancellation.cancel();
    let error = worker
        .join()
        .expect("execution thread")
        .expect_err("cancel running command");

    let ExecuteError::MayHaveStarted(MayHaveStartedError {
        kind: MayHaveStartedKind::Cancelled,
        partial_output,
    }) = error
    else {
        panic!("running cancellation must be classified as may-have-started");
    };
    assert!(partial_output.stdout.contains("started"));
}

#[cfg(unix)]
#[test]
fn timeout_terminates_descendant_processes() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let options = ExecuteOptions::default()
        .with_cwd(temp.path())
        .with_timeout(Duration::from_millis(100));

    let error = execute_with_options("sh -c 'sleep 30 & echo $! > child.pid; wait'", &options)
        .expect_err("timeout command tree");
    assert!(matches!(
        error,
        ExecuteError::MayHaveStarted(MayHaveStartedError {
            kind: MayHaveStartedKind::TimedOut,
            ..
        })
    ));

    let pid = fs::read_to_string(temp.path().join("child.pid"))
        .expect("child pid")
        .trim()
        .to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while process_exists(&pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_exists(&pid), "descendant {pid} survived timeout");
}

#[cfg(unix)]
#[test]
fn stdout_and_stderr_capture_are_independently_bounded() {
    let options = ExecuteOptions::default().with_output_limit(128);
    let command = "head -c 4096 /dev/zero | tr '\\0' x; head -c 4096 /dev/zero | tr '\\0' y >&2";

    let result = execute_with_options(command, &options).expect("capture bounded output");

    assert_eq!(result.stdout.len(), 128);
    assert_eq!(result.stderr.len(), 128);
    assert!(result.stdout_truncated);
    assert!(result.stderr_truncated);
}

#[test]
fn git_and_cargo_routes_filter_in_process() {
    let git = tempfile::tempdir().expect("git directory");
    run_in(git.path(), "git", &["init", "-q"]);
    run_in(
        git.path(),
        "git",
        &["config", "user.email", "rtk@example.com"],
    );
    run_in(git.path(), "git", &["config", "user.name", "RTK Test"]);
    fs::write(git.path().join("file.txt"), "one\n").expect("write git file");
    run_in(git.path(), "git", &["add", "file.txt"]);
    run_in(git.path(), "git", &["commit", "-qm", "initial"]);
    fs::write(git.path().join("file.txt"), "one\ntwo\n").expect("modify git file");

    for command in ["git status", "git diff", "git log -1", "git show HEAD"] {
        let result = execute_with_options(command, &ExecuteOptions::default().with_cwd(git.path()))
            .expect("execute git filter");
        assert_eq!(result.exit_code, 0, "{command}: {}", result.stderr);
        assert_eq!(result.route, ExecutionRoute::Filtered { tool: "git" });
    }

    let cargo = tempfile::tempdir().expect("cargo directory");
    fs::create_dir(cargo.path().join("src")).expect("create src");
    fs::write(
        cargo.path().join("Cargo.toml"),
        "[package]\nname='rtk-integration'\nversion='0.1.0'\nedition='2021'\n",
    )
    .expect("write manifest");
    fs::write(
        cargo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .expect("write source");
    let result = execute_with_options(
        "cargo check",
        &ExecuteOptions::default().with_cwd(cargo.path()),
    )
    .expect("execute cargo filter");
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.route, ExecutionRoute::Filtered { tool: "cargo" });
}

#[test]
fn safe_chains_filter_but_complex_shell_syntax_passes_through() {
    let temp = tempfile::tempdir().expect("temporary directory");
    fs::write(temp.path().join("one.txt"), "one\n").expect("write one");
    fs::write(temp.path().join("two.txt"), "one\ntwo\n").expect("write two");
    let options = ExecuteOptions::default().with_cwd(temp.path());

    let chain = execute_with_options("wc -l missing || wc -l one.txt && wc -l two.txt", &options)
        .expect("execute safe chain");
    assert_eq!(chain.exit_code, 0);
    assert_eq!(chain.stdout, "1\n2");
    assert!(chain.stderr.contains("missing"));

    let pipeline =
        execute_with_options("printf 'one\\ntwo\\n' | wc -l", &options).expect("execute pipeline");
    assert_eq!(pipeline.stdout.trim(), "2");
    assert_eq!(pipeline.route, ExecutionRoute::ShellPassthrough);

    let redirected =
        execute_with_options("wc -l one.txt > count.txt", &options).expect("execute redirect");
    assert_eq!(redirected.route, ExecutionRoute::ShellPassthrough);
    assert!(fs::read_to_string(temp.path().join("count.txt"))
        .expect("redirect output")
        .contains('1'));
}

fn run_in(cwd: &Path, program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("start fixture command");
    assert!(status.success(), "fixture command failed: {program}");
}

#[cfg(unix)]
fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .is_ok_and(|status| status.success())
}
