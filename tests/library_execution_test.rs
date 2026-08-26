use rtk::{execute_with_options, ExecuteOptions, ExecutionRoute};
use std::fs;

#[test]
fn public_api_executes_and_filters_a_command() {
    let temp = tempfile::tempdir().expect("temporary directory");
    fs::write(temp.path().join("input.txt"), "one\ntwo\nthree\n").expect("write fixture");
    let options = ExecuteOptions {
        cwd: Some(temp.path().to_path_buf()),
        tracking: false,
    };

    let result = execute_with_options("wc -l input.txt", &options).expect("execute command");

    assert_eq!(result.stdout.trim(), "3");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.route, ExecutionRoute::Filtered { tool: "wc" });
}
