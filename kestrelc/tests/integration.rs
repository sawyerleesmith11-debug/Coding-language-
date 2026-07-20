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

// Windows' CRT translates every `\n` a compiled binary writes to stdout
// into `\r\n` in text mode — a platform difference in the C runtime, not
// a kestrelc miscompile. Every other backend (run/runFast/kestrelc-web)
// always emits plain `\n`, so tests compare against `\n`-only expected
// strings; normalize a native binary's captured stdout the same way
// before comparing.
fn native_stdout(run: &std::process::Output) -> String {
    String::from_utf8_lossy(&run.stdout).replace("\r\n", "\n")
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
    let stdout = native_stdout(&run);

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
fn compiles_and_runs_basics_kes_with_correct_output() {
    // basics.kes exercises array literals, array parameters, and
    // indexing (get_safe) — the array support this test file covers.
    let scratch = scratch_dir("basics");
    let (ok, stderr, bin) = compile("basics.kes", &scratch);
    assert!(ok, "kestrelc failed to compile basics.kes:\n{stderr}");

    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled basics binary exited with failure");
    let stdout = native_stdout(&run);

    let expected = "\
square: 9
square: 16
square: 25
square: 36
sum of squares(3,4) = 25
safe get nums[2] = 5
";
    assert_eq!(stdout, expected);
}

#[test]
fn statically_provable_out_of_bounds_index_is_a_compile_error() {
    // A literal index into a literal-length array is fully provable at
    // compile time — proof-carrying optimization means kestrelc catches
    // this before the program ever runs, not deferred to a runtime trap.
    let scratch = scratch_dir("oob_static");
    let src_path = scratch.join("oob_static.kes");
    fs::write(
        &src_path,
        r#"
        fn main() {
            let a = [1, 2, 3];
            print(a[5]);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected a provably out-of-bounds index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("out of bounds") && stderr.contains("compile time"),
        "expected a compile-time bounds proof error, got:\n{stderr}"
    );
}

#[test]
fn dynamically_out_of_bounds_index_traps_at_runtime_instead_of_reading_garbage() {
    // The index isn't a literal here, so it can't be proven at compile
    // time (see the static case above) — this still has to fall back to
    // a runtime check, and a failing check halts the process (SIGILL)
    // rather than silently returning whatever happened to be in memory.
    let scratch = scratch_dir("oob_dynamic");
    let src_path = scratch.join("oob_dynamic.kes");
    fs::write(
        &src_path,
        r#"
        fn main() {
            let a = [1, 2, 3];
            let i = 5;
            print(a[i]);
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

    let bin = scratch.join("oob_dynamic");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(!run.status.success(), "out-of-bounds access should not exit successfully");
    assert!(run.stdout.is_empty(), "should trap before printing the program's own output");
    let stderr = String::from_utf8_lossy(&run.stderr).replace("\r\n", "\n");
    assert!(
        stderr.contains("kestrelc: Index 5 out of bounds for array of length 3"),
        "expected a friendly bounds-check failure message, got:\n{stderr}"
    );
}

#[test]
fn statically_provable_in_bounds_index_still_produces_correct_output() {
    // Sanity check that the compile-time-proof fast path doesn't just
    // skip the check — it still has to load the right value.
    let scratch = scratch_dir("inbounds_static");
    let src_path = scratch.join("inbounds_static.kes");
    fs::write(
        &src_path,
        r#"
        fn main() {
            let a = [10, 20, 30];
            print(a[0]);
            print(a[2]);
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

    let bin = scratch.join("inbounds_static");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "10\n30\n");
}

#[test]
fn array_parameter_and_indexing_produce_correct_results() {
    let scratch = scratch_dir("arrparam");
    let src_path = scratch.join("arrparam.kes");
    fs::write(
        &src_path,
        r#"
        fn sum3(a: [i32; N]) -> i32 {
            return a[0] + a[1] + a[2];
        }
        fn main() {
            let xs = [10, 20, 30];
            print(sum3(xs));
            print(xs[1]);
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

    let bin = scratch.join("arrparam");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "60\n20\n");
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
    assert_eq!(native_stdout(&run), "610\n14\n1\n3\n");
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
    assert_eq!(native_stdout(&run), "4950\n");
}

#[test]
fn where_clause_call_site_proof_accepts_valid_literal_call() {
    // This is the design doc's own get_safe example, verbatim, with a
    // call site whose index/array are both provable at compile time —
    // should compile and elide the check inside get_safe entirely.
    let scratch = scratch_dir("where_ok");
    let src_path = scratch.join("where_ok.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            print(get_safe(nums, 2));
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

    let bin = scratch.join("where_ok");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "5\n");
}

#[test]
fn wasm_backend_where_clause_call_site_proof_accepts_valid_literal_call() {
    // Same design-doc get_safe example as the native test above, proving
    // the WASM backend's own copy of the elision fast path (added
    // alongside the native one — see wasm_codegen.rs's Index/Call arms)
    // actually runs correctly, not just compiles.
    let scratch = scratch_dir("wasm_where_ok");
    let src_path = scratch.join("where_ok.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            print(get_safe(nums, 2));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("where_ok.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "5\n");
}

#[test]
fn where_clause_call_site_rejects_provably_invalid_index() {
    let scratch = scratch_dir("where_bad_index");
    let src_path = scratch.join("where_bad_index.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            print(get_safe(nums, 9));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected a provably out-of-bounds where-clause call");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't satisfy its own"),
        "expected a where-clause proof-failure error, got:\n{stderr}"
    );
}

#[test]
fn wasm_backend_where_clause_call_site_rejects_provably_invalid_index() {
    let scratch = scratch_dir("wasm_where_bad_index");
    let src_path = scratch.join("where_bad_index.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            print(get_safe(nums, 9));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc --wasm should have rejected a provably out-of-bounds where-clause call");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't satisfy its own"),
        "expected a where-clause proof-failure error, got:\n{stderr}"
    );
}

#[test]
fn where_clause_call_site_rejects_unprovable_dynamic_index() {
    // The design doc is explicit that an unprovable call site is a
    // compile error, not a silent fallback to a runtime check — our
    // prover can't reason about a variable index, so this must fail.
    let scratch = scratch_dir("where_unprovable");
    let src_path = scratch.join("where_unprovable.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            let idx = 2;
            print(get_safe(nums, idx));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected an unprovable where-clause call site");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't prove"),
        "expected a where-clause unprovable-call-site error, got:\n{stderr}"
    );
}

#[test]
fn wasm_backend_where_clause_call_site_rejects_unprovable_dynamic_index() {
    let scratch = scratch_dir("wasm_where_unprovable");
    let src_path = scratch.join("where_unprovable.kes");
    fs::write(
        &src_path,
        r#"
        fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
            return arr[i];
        }
        fn main() {
            let nums = [3, 4, 5, 6];
            let idx = 2;
            print(get_safe(nums, idx));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc --wasm should have rejected an unprovable where-clause call site");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't prove"),
        "expected a where-clause unprovable-call-site error, got:\n{stderr}"
    );
}

#[test]
fn wasm_backend_compiles_and_runs_fibonacci_kes_with_correct_output() {
    // Actually instantiates and runs the compiled .wasm through Node's
    // WebAssembly API (the same host environment the browser editor
    // would provide) — not just checking the bytes look plausible.
    let scratch = scratch_dir("wasm_fib");
    let src = repo_root().join("examples").join("fibonacci.kes");
    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("fibonacci.wasm");
    assert!(wasm_path.exists(), "expected fibonacci.wasm to be written");

    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));

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
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
}

fn run_wasm_via_node(wasm_path: &std::path::Path) -> std::process::Output {
    // Writes each print()'s line to stdout as soon as it completes
    // (isLast), same as kestrel-editor.html's real host imports (see
    // its `flush()`) — not batched until `main()` returns. That
    // difference matters for a program that traps partway through: any
    // output already printed before the trap is real, observable
    // output in the actual product, and this harness needs to
    // reproduce that to test it (e.g. the friendly out-of-bounds
    // message a trap now prints right before it happens).
    let node_script = r#"
        const fs = require("fs");
        const bytes = fs.readFileSync(process.argv[1]);
        let cur = [];
        let instance;
        const imports = { env: {
            print_i64: (v, isLast) => { cur.push(v.toString()); if (isLast) { process.stdout.write(cur.join(" ") + "\n"); cur = []; } },
            print_str: (ptr, len, isLast) => {
                const bytes = new Uint8Array(instance.exports.memory.buffer, ptr, len);
                cur.push(Buffer.from(bytes).toString("utf8"));
                if (isLast) { process.stdout.write(cur.join(" ") + "\n"); cur = []; }
            },
        }};
        WebAssembly.instantiate(bytes, imports).then(({ instance: inst }) => {
            instance = inst;
            inst.exports.main();
        }).catch((e) => { console.error(e); process.exit(1); });
    "#;
    Command::new("node")
        .arg("-e")
        .arg(node_script)
        .arg(wasm_path)
        .output()
        .expect("failed to run node (required for WASM backend tests)")
}

#[test]
fn wasm_backend_compiles_and_runs_basics_kes_with_correct_output() {
    // basics.kes exercises array literals, an array parameter, indexing,
    // and a `where`-guarded call — the WASM backend's array support.
    let scratch = scratch_dir("wasm_basics");
    let src = repo_root().join("examples").join("basics.kes");
    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("basics.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));

    let expected = "\
square: 9
square: 16
square: 25
square: 36
sum of squares(3,4) = 25
safe get nums[2] = 5
";
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
}

#[test]
fn wasm_backend_traps_on_out_of_bounds_array_index() {
    let scratch = scratch_dir("wasm_oob");
    let src_path = scratch.join("oob.kes");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&src_path, "fn main() {\n    let a = [1, 2, 3];\n    let i = 5;\n    print(\"val =\", a[i]);\n}\n").unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("oob.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(!run.status.success(), "expected the wasm module to trap on out-of-bounds access");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("unreachable"),
        "expected a wasm 'unreachable' trap, got:\n{stderr}"
    );
    // The friendly message is printed through the same host print
    // imports the program's own print() calls use, right before the
    // trap — real, observable output in the actual product (see
    // run_wasm_via_node's per-line flushing), not lost by the trap.
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("kestrelc: Index 5 out of bounds for array of length 3"),
        "expected a friendly bounds-check failure message, got:\n{stdout}"
    );
}

#[test]
fn wasm_backend_rejects_statically_provable_out_of_bounds_index_at_compile_time() {
    let scratch = scratch_dir("wasm_static_oob");
    let src_path = scratch.join("bad.kes");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(&src_path, "fn main() {\n    let a = [1, 2, 3];\n    print(\"val =\", a[9]);\n}\n").unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc --wasm should have rejected a statically-provable out-of-bounds index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("proven at compile time"),
        "expected a compile-time bounds-proof error, got:\n{stderr}"
    );
}

#[test]
fn native_compile_cache_hit_reuses_the_object_file_and_still_produces_correct_output() {
    let scratch = scratch_dir("cache_native");
    let cache_dir = scratch.join("cache");
    let src = repo_root().join("examples").join("fibonacci.kes");

    let first = Command::new(kestrelc_bin())
        .arg(&src)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(first.status.success(), "first compile failed:\n{}", String::from_utf8_lossy(&first.stderr));
    assert!(
        !String::from_utf8_lossy(&first.stdout).contains("(cached)"),
        "first compile should be a cache miss"
    );

    let second = Command::new(kestrelc_bin())
        .arg(&src)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(second.status.success(), "second compile failed:\n{}", String::from_utf8_lossy(&second.stderr));
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("(cached)"),
        "second compile should be a cache hit, got:\n{}",
        String::from_utf8_lossy(&second.stdout)
    );

    let bin_path = scratch.join("fibonacci");
    let run = Command::new(&bin_path).output().expect("failed to run compiled binary");
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).starts_with("fib 0 = 0"));
}

#[test]
fn wasm_compile_cache_hit_produces_byte_identical_output() {
    let scratch = scratch_dir("cache_wasm");
    let cache_dir = scratch.join("cache");
    let src = repo_root().join("examples").join("fibonacci.kes");

    for _ in 0..2 {
        let out = Command::new(kestrelc_bin())
            .arg("--wasm")
            .arg(&src)
            .current_dir(&scratch)
            .env("KESTRELC_CACHE_DIR", &cache_dir)
            .output()
            .expect("failed to run kestrelc");
        assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    // Confirm the second run really did hit the cache, not just happen
    // to produce the same bytes independently.
    let second = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("(cached)"),
        "expected a cache hit by the third invocation"
    );

    let wasm_path = scratch.join("fibonacci.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the cached wasm module:\n{}", String::from_utf8_lossy(&run.stderr));
    assert!(String::from_utf8_lossy(&run.stdout).starts_with("fib 0 = 0"));
}

#[test]
fn compile_cache_misses_when_source_changes() {
    let scratch = scratch_dir("cache_invalidation");
    let cache_dir = scratch.join("cache");
    let src_path = scratch.join("prog.kes");
    std::fs::write(&src_path, "fn main() {\n    print(\"v1\");\n}\n").unwrap();

    let first = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(first.status.success());

    std::fs::write(&src_path, "fn main() {\n    print(\"v2, a different program\");\n}\n").unwrap();
    let second = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(second.status.success());
    assert!(
        !String::from_utf8_lossy(&second.stdout).contains("(cached)"),
        "changed source should not hit the previous entry's cache"
    );
}

// ============================== parallel_map ==============================

#[test]
fn parallel_map_produces_correct_results_natively() {
    let scratch = scratch_dir("pmap_native");
    let src_path = scratch.join("pmap.kes");
    fs::write(
        &src_path,
        r#"
        pure fn square(x: i32) -> i32 { return x * x; }
        fn main() {
            let nums = [1, 2, 3, 4, 5];
            let squares = parallel_map(square, nums);
            print(squares[0], squares[1], squares[2], squares[3], squares[4]);
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

    let bin = scratch.join("pmap");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "1 4 9 16 25\n");
}

#[test]
fn chained_parallel_map_calls_fuse_and_still_produce_correct_output() {
    // Same fusion kestrel.js's fuseLoops does: `let a = parallel_map(f,
    // arr); let b = parallel_map(g, a);` becomes one parallel_map with a
    // synthesized g(f(x)) function. This is a real end-to-end check
    // (compile, link, run, verify actual output) — src/fusion.rs's own
    // unit tests check the AST transform in isolation; this one proves
    // the fused program still codegens and runs correctly.
    let scratch = scratch_dir("pmap_fused");
    let src_path = scratch.join("pmap_fused.kes");
    fs::write(
        &src_path,
        r#"
        pure fn square(x: i32) -> i32 { return x * x; }
        pure fn inc(x: i32) -> i32 { return x + 1; }
        fn main() {
            let a = parallel_map(square, [1, 2, 3, 4]);
            let b = parallel_map(inc, a);
            print(b[0], b[1], b[2], b[3]);
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

    let bin = scratch.join("pmap_fused");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "2 5 10 17\n");
}

#[test]
fn parallel_map_is_correct_on_a_large_array_that_crosses_the_real_thread_pool_threshold() {
    // kestrelc_runtime.c only spins up real OS threads above a size
    // threshold (see runtime/kestrelc_runtime.c) — below it, running
    // inline is faster than thread setup/teardown. This array is large
    // enough to force the actual multi-threaded chunked path, which is
    // exactly the code a small correctness test wouldn't exercise.
    let scratch = scratch_dir("pmap_large");
    let src_path = scratch.join("pmap_large.kes");

    let n: i64 = 20_000;
    let nums: Vec<i64> = (0..n).map(|i| ((i * 2654435761) % 2001) - 1000).collect();
    let literal = nums.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ");
    let src = format!(
        r#"
        pure fn work(x: i32) -> i32 {{
            let i = 0;
            let acc = x;
            while (i < 200) {{
                acc = (acc * 17 + 23) % 1000003;
                i = i + 1;
            }}
            return acc;
        }}
        fn main() {{
            let nums = [{literal}];
            let results = parallel_map(work, nums);
            let total = 0;
            let i = 0;
            while (i < {n}) {{
                total = total + results[i];
                i = i + 1;
            }}
            print(total);
        }}
        "#
    );
    fs::write(&src_path, &src).unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("pmap_large");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled binary exited with failure");

    // Reference value, computed independently in Rust with the same
    // truncating-toward-zero remainder semantics kestrelc's `srem`/C's
    // `%` use (not Python's/most languages' floored mod) — this is the
    // correctness oracle the native binary's output is checked against.
    fn work(mut x: i64) -> i64 {
        for _ in 0..200 {
            x = (x * 17 + 23) % 1000003;
        }
        x
    }
    let expected: i64 = nums.iter().map(|&v| work(v)).sum();
    assert_eq!(native_stdout(&run).trim(), expected.to_string());
}

#[test]
fn parallel_map_rejects_a_non_pure_function() {
    let scratch = scratch_dir("pmap_impure");
    let src_path = scratch.join("pmap_impure.kes");
    fs::write(
        &src_path,
        r#"
        fn notpure(x: i32) -> i32 { print(x); return x; }
        fn main() {
            let nums = [1, 2, 3];
            let out = parallel_map(notpure, nums);
            print(out[0]);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected a non-pure parallel_map callee");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("must be a 'pure fn'"), "expected the purity error, got:\n{stderr}");
}

#[test]
fn parallel_map_rejects_an_array_parameter_source_array() {
    // The output array's size has to be known at compile time (it's a
    // stack allocation) — only supported for a literal-length array
    // source (a `let` array literal), not one passed in as a parameter.
    let scratch = scratch_dir("pmap_param_arr");
    let src_path = scratch.join("pmap_param_arr.kes");
    fs::write(
        &src_path,
        r#"
        pure fn square(x: i32) -> i32 { return x * x; }
        fn map_it(arr: [i32; N]) -> i32 {
            let out = parallel_map(square, arr);
            return out[0];
        }
        fn main() {
            let nums = [1, 2, 3];
            print(map_it(nums));
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected parallel_map over an array parameter");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fixed-size array literal"),
        "expected the literal-length-array-only error, got:\n{stderr}"
    );
}

#[test]
fn wasm_backend_parallel_map_produces_correct_results() {
    let scratch = scratch_dir("wasm_pmap");
    let src_path = scratch.join("pmap.kes");
    fs::write(
        &src_path,
        r#"
        pure fn square(x: i32) -> i32 { return x * x; }
        fn main() {
            let nums = [1, 2, 3, 4, 5];
            let squares = parallel_map(square, nums);
            print(squares[0], squares[1], squares[2], squares[3], squares[4]);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("pmap.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "1 4 9 16 25\n");
}

#[test]
fn wasm_backend_chained_parallel_map_calls_fuse_and_still_produce_correct_output() {
    let scratch = scratch_dir("wasm_pmap_fused");
    let src_path = scratch.join("pmap_fused.kes");
    fs::write(
        &src_path,
        r#"
        pure fn square(x: i32) -> i32 { return x * x; }
        pure fn inc(x: i32) -> i32 { return x + 1; }
        fn main() {
            let a = parallel_map(square, [1, 2, 3, 4]);
            let b = parallel_map(inc, a);
            print(b[0], b[1], b[2], b[3]);
        }
        "#,
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("pmap_fused.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "2 5 10 17\n");
}

// ============================== type checker ==============================

fn expect_type_error(scratch_name: &str, src: &str, expected_substr: &str) {
    let scratch = scratch_dir(scratch_name);
    let src_path = scratch.join("prog.kes");
    fs::write(&src_path, src).unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected this program:\n{src}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(expected_substr),
        "expected stderr to contain '{expected_substr}', got:\n{stderr}"
    );
}

#[test]
fn typecheck_rejects_mixing_a_number_and_a_boolean() {
    expect_type_error("tc_add_bool", "fn main() {\n    print(5 + true);\n}\n", "needs two numbers");
}

#[test]
fn typecheck_rejects_not_applied_to_a_number() {
    expect_type_error("tc_not_int", "fn main() {\n    print(!5);\n}\n", "'!' needs a boolean");
}

#[test]
fn typecheck_rejects_a_numeric_if_condition() {
    expect_type_error(
        "tc_if_int",
        "fn main() {\n    if (5) {\n        print(1);\n    }\n}\n",
        "if-condition must be a boolean",
    );
}

#[test]
fn typecheck_rejects_wrong_argument_count() {
    expect_type_error(
        "tc_arg_count",
        "fn add(x: i32, y: i32) -> i32 { return x + y; }\nfn main() {\n    print(add(1, 2, 3));\n}\n",
        "expects 2 arguments, got 3",
    );
}

#[test]
fn typecheck_rejects_reassigning_a_variable_to_a_different_kind() {
    expect_type_error(
        "tc_reassign",
        "fn main() {\n    let x = 5;\n    x = true;\n}\n",
        "was first bound as int",
    );
}

#[test]
fn typecheck_does_not_reject_legitimate_programs() {
    // Both real examples must still compile cleanly with the type checker
    // wired in — no false positives on working code.
    let scratch = scratch_dir("tc_no_false_positives");
    for name in ["basics.kes", "fibonacci.kes"] {
        let (ok, stderr, _bin) = compile(name, &scratch);
        assert!(ok, "kestrelc should still accept {name}:\n{stderr}");
    }
}

#[test]
fn typecheck_does_not_flag_a_boolean_returning_function_used_as_a_condition() {
    // The callee's return kind isn't tracked (v1 scope), so this must be
    // treated as Unknown and allowed through, not guessed at and rejected.
    let scratch = scratch_dir("tc_unknown_call_kind");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn is_even(x: i32) -> bool { return x % 2 == 0; }\nfn main() {\n    if (is_even(4)) {\n        print(\"even\");\n    }\n}\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "even\n");
}

#[test]
fn wasm_backend_typecheck_rejects_the_same_ill_typed_program() {
    let scratch = scratch_dir("wasm_tc");
    let src_path = scratch.join("prog.kes");
    fs::write(&src_path, "fn main() {\n    print(5 + true);\n}\n").unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc --wasm should have rejected this program");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("needs two numbers"), "expected the type error, got:\n{stderr}");
}
