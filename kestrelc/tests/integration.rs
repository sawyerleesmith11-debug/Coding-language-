// End-to-end tests: actually compile a .kes file, link it, run the
// resulting binary, and check its output — not just "did codegen not
// crash." Runs in a scratch temp directory so it never leaves build
// artifacts in the repo.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn kestrelc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kestrelc"))
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kestrelc-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn repo_root() -> PathBuf {
    // tests/ is directly under kestrelc/, which is directly under the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Compiles `examples/<name>` with kestrelc inside a scratch dir and
/// returns (compile succeeded?, compiler stderr, path to the produced
/// binary if compilation succeeded).
fn compile(name: &str, scratch: &PathBuf) -> (bool, String, PathBuf) {
    let src = repo_root().join("examples").join(name);
    let out = Command::new(kestrelc_bin())
        .arg(&src)
        .current_dir(scratch)
        .output()
        .expect("failed to run kestrelc");
    let stem = src.file_stem().unwrap().to_string_lossy().to_string();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string(), scratch.join(stem))
}

#[test]
fn compiles_and_runs_fibonacci_kes_with_correct_output() {
    let scratch = scratch_dir("fibonacci");
    let (ok, stderr, bin) = compile("fibonacci.kes", &scratch);
    assert!(ok, "kestrelc failed to compile fibonacci.kes:\n{stderr}");

    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled fibonacci binary exited with failure");
    let stdout = String::from_utf8_lossy(&run.stdout);

    // Same expected output as kestrel.test.js's fibonacci.kes test and
    // the JS backends — all three backends must agree.
    let expected = "\
fib 0 = 0
fib 1 = 1
fib 2 = 1
fib 3 = 2
fib 4 = 3
fib 5 = 5
fib 6 = 8
fib 7 = 13
fib 8 = 21
fib 9 = 34
";
    assert_eq!(stdout, expected);
}

#[test]
fn rejects_purity_violation_kes_at_compile_time() {
    let scratch = scratch_dir("purity");
    let (ok, stderr, _bin) = compile("purity_violation.kes", &scratch);
    assert!(!ok, "kestrelc should have rejected purity_violation.kes");
    assert!(
        stderr.contains("'oops' is marked pure"),
        "expected the purity error message, got:\n{stderr}"
    );
}

#[test]
fn rejects_array_usage_with_a_clear_error_not_a_crash() {
    // Arrays aren't supported yet (see kestrelc/README.md) — basics.kes
    // uses them, so it must fail cleanly, not silently miscompile or panic.
    let scratch = scratch_dir("basics");
    let (ok, stderr, _bin) = compile("basics.kes", &scratch);
    assert!(!ok, "kestrelc should have refused to compile array usage");
    assert!(
        stderr.contains("doesn't support arrays yet"),
        "expected the arrays-not-supported error, got:\n{stderr}"
    );
}

#[test]
fn recursion_and_arithmetic_produce_correct_results() {
    // A small standalone program (not one of the shared examples/) that
    // exercises recursion, arithmetic, comparisons, and both branches of
    // an if/else — written directly to the scratch dir.
    let scratch = scratch_dir("adhoc");
    let src_path = scratch.join("adhoc.kes");
    fs::write(
        &src_path,
        r#"
        fn fib(n: i32) -> i32 {
            if (n < 2) { return n; } else { return fib(n - 1) + fib(n - 2); }
        }
        fn main() {
            print(fib(15));
            print(2 + 3 * 4);
            let x = 10;
            let y = 3;
            print(x % y);
            print(x / y);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("adhoc");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "610\n14\n1\n3\n");
}

#[test]
fn while_loop_with_mutation_produces_correct_result() {
    let scratch = scratch_dir("loopmut");
    let src_path = scratch.join("loopmut.kes");
    fs::write(
        &src_path,
        r#"
        fn main() {
            let i = 0;
            let total = 0;
            while (i < 100) {
                total = total + i;
                i = i + 1;
            }
            print(total);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("loopmut");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "4950\n");
}
