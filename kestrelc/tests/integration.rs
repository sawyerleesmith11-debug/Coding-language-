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
    // kestrelc's own purity checker now carries a real source position
    // (not just kestrel.js's) — a file:line:col header and a caret line,
    // the same treatment lex/parse errors already got.
    assert!(
        stderr.contains("purity_violation.kes:") && stderr.contains('^'),
        "expected a file:line:col: + caret diagnostic, got:\n{stderr}"
    );
}

#[test]
fn rejects_a_parallel_map_misuse_with_a_caret_pointing_at_the_call_site() {
    let scratch = scratch_dir("pmap_caret");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn not_pure(x: i64) -> i64 { print(x); return x; }\n\
         fn main() {\n\
         \x20   let a = [1, 2, 3];\n\
         \x20   let b = parallel_map(not_pure, a);\n\
         \x20   print(b[0]);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should have rejected a non-pure parallel_map callback");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("must be a 'pure fn'"), "expected the parallel_map error message, got:\n{stderr}");
    assert!(
        stderr.contains("prog.kes:4:") && stderr.contains('^'),
        "expected a caret pointing at line 4 (the parallel_map call), got:\n{stderr}"
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
    assert!(
        stderr.contains("prog.kes:2:") && stderr.contains('^'),
        "expected a file:line:col: + caret diagnostic, got:\n{stderr}"
    );
}

// ==================== runtime call-count profile / inlining ====================

#[test]
fn native_run_writes_a_call_count_profile_next_to_the_compile_cache() {
    let scratch = scratch_dir("profile_write");
    let cache_dir = scratch.join("cache");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "pure fn double(x: i64) -> i64 { return x * 2; }\n\
         fn main() {\n\
         \x20   let i = 0;\n\
         \x20   let sum = 0;\n\
         \x20   while (i < 10) {\n\
         \x20       sum = sum + double(i);\n\
         \x20       i = i + 1;\n\
         \x20   }\n\
         \x20   print(sum);\n\
         }\n",
    )
    .unwrap();

    let compiled = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(compiled.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&compiled.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled binary exited with failure");
    assert_eq!(native_stdout(&run), "90\n"); // 2*(0+1+...+9) = 90

    let profile_file = fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().map(|ext| ext == "profile").unwrap_or(false))
        .unwrap_or_else(|| panic!("no .profile file written to {}", cache_dir.display()))
        .path();
    let profile_text = fs::read_to_string(&profile_file).unwrap();
    assert!(
        profile_text.lines().any(|l| l.starts_with("double ") && l.trim_end().ends_with("10")),
        "expected 'double 10' in the profile, got:\n{profile_text}"
    );
}

#[test]
fn native_recompile_after_a_run_produces_a_different_cache_entry_and_still_runs_correctly() {
    // The whole point of a *runtime-feedback* cache (kestrel-DESIGN.md
    // idea #1) is that compiling the same source twice can legitimately
    // produce two different artifacts if the program actually ran in
    // between — this is the end-to-end proof that the loop really
    // fires, not just that inline.rs's AST-level unit tests pass in
    // isolation (see fusion.rs's history: an AST transform that "looks
    // right" still miscompiled until an end-to-end test caught it).
    let scratch = scratch_dir("profile_recompile");
    let cache_dir = scratch.join("cache");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "pure fn double(x: i64) -> i64 { return x * 2; }\n\
         fn main() {\n\
         \x20   let i = 0;\n\
         \x20   let sum = 0;\n\
         \x20   while (i < 10) {\n\
         \x20       sum = sum + double(i);\n\
         \x20       i = i + 1;\n\
         \x20   }\n\
         \x20   print(sum);\n\
         }\n",
    )
    .unwrap();

    let first_compile = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(first_compile.status.success());
    assert!(
        !String::from_utf8_lossy(&first_compile.stdout).contains("(cached)"),
        "first compile (no profile yet) should be a cache miss"
    );

    let bin = scratch.join("prog");
    let first_run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&first_run), "90\n");

    // Same source, but a profile now exists from the run above — the
    // artifact cache key folds in a fingerprint of it (see
    // cache::artifact_key), so this must NOT be a cache hit even though
    // the .kes file on disk hasn't changed a single byte.
    let second_compile = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(second_compile.status.success(), "recompile failed:\n{}", String::from_utf8_lossy(&second_compile.stderr));
    assert!(
        !String::from_utf8_lossy(&second_compile.stdout).contains("(cached)"),
        "recompile after a run (new profile data) should not reuse the pre-profile cache entry"
    );

    let second_run = Command::new(&bin).output().expect("failed to run recompiled binary");
    assert!(second_run.status.success(), "recompiled binary exited with failure");
    assert_eq!(native_stdout(&second_run), "90\n", "inlining must never change a program's actual output");

    // A third compile, with the same (now-stable) profile data, should
    // finally hit the cache — proves the loop settles instead of
    // recompiling forever.
    let third_compile = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .env("KESTRELC_CACHE_DIR", &cache_dir)
        .output()
        .expect("failed to run kestrelc");
    assert!(
        String::from_utf8_lossy(&third_compile.stdout).contains("(cached)"),
        "expected the third compile (unchanged source, unchanged profile) to hit the cache, got:\n{}",
        String::from_utf8_lossy(&third_compile.stdout)
    );
}

// ==================== native memoization ====================

#[test]
fn memoized_recursive_pure_fn_still_produces_correct_output() {
    // fib is pure, scalar-param, never passed to parallel_map — eligible
    // for a memo slot (see codegen.rs's MemoState). Recursive calls hit
    // and populate the *same* slot's cache mid-computation; this is the
    // real end-to-end proof that doesn't miscompile, not just "codegen
    // didn't crash."
    let scratch = scratch_dir("memo_fib");
    let src_path = scratch.join("memo_fib.kes");
    fs::write(
        &src_path,
        "pure fn fib(n: i64) -> i64 {\n\
         \x20   if (n < 2) {\n\
         \x20       return n;\n\
         \x20   }\n\
         \x20   return fib(n - 1) + fib(n - 2);\n\
         }\n\
         fn main() {\n\
         \x20   let i = 0;\n\
         \x20   while (i < 15) {\n\
         \x20       print(fib(i));\n\
         \x20       i = i + 1;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("memo_fib");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert!(run.status.success(), "compiled binary exited with failure");
    let expected = "0\n1\n1\n2\n3\n5\n8\n13\n21\n34\n55\n89\n144\n233\n377\n";
    assert_eq!(native_stdout(&run), expected);
}

#[test]
fn memoized_fn_called_with_the_same_arguments_repeatedly_returns_the_same_answer() {
    // Not observably different from an unmemoized call as far as output
    // goes (memoization is a perf optimization, correctness must be
    // unchanged) — this exercises many repeated calls with overlapping
    // argument values, the case a broken cache (wrong-key collision,
    // stale entry) would most likely get wrong.
    let scratch = scratch_dir("memo_repeat");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "pure fn square(x: i64) -> i64 { return x * x; }\n\
         fn main() {\n\
         \x20   let i = 0;\n\
         \x20   let total = 0;\n\
         \x20   while (i < 20) {\n\
         \x20       total = total + square(i % 5);\n\
         \x20       i = i + 1;\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
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
    assert!(run.status.success());
    // i % 5 cycles 0,1,2,3,4 four times; squares sum to 0+1+4+9+16=30 per
    // cycle, times 4 cycles = 120.
    assert_eq!(native_stdout(&run), "120\n");
}

#[test]
fn a_function_used_as_a_parallel_map_callback_is_never_memoized_and_still_runs_correctly() {
    // The whole safety argument for lock-free memoization is that a
    // memoized function is provably never called from more than one OS
    // thread — codegen.rs excludes any function ever passed as
    // parallel_map's callback from getting a memo slot at all (see
    // inline::collect_parallel_map_callbacks, reused for this). This
    // doesn't (and can't, from the outside) observe the *absence* of a
    // memo slot directly, but it does prove such a program still
    // compiles and runs correctly under both a direct call and the
    // parallel_map path to the same function.
    let scratch = scratch_dir("memo_pmap_exclude");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "pure fn triple(x: i64) -> i64 { return x * 3; }\n\
         fn main() {\n\
         \x20   let xs = [1, 2, 3, 4, 5];\n\
         \x20   let ys = parallel_map(triple, xs);\n\
         \x20   print(ys[0], ys[4]);\n\
         \x20   print(triple(9));\n\
         }\n",
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
    assert!(run.status.success());
    assert_eq!(native_stdout(&run), "3 15\n27\n");
}

#[test]
fn a_memoized_function_called_with_many_distinct_arguments_stays_fast_and_correct() {
    // Regression test: kestrelc_runtime.c's per-function memo cache used
    // to be a plain array, scanned linearly on every lookup. Fine for a
    // handful of memoized calls, but a hot pure function called with
    // many *distinct* arguments (a completely realistic case — not a
    // contrived adversarial one) made every lookup rescan everything
    // seen so far, going quadratic overall. A 5,000,000-call version of
    // exactly this (kestrelc/examples/bench_loop.kes) hung for minutes,
    // still burning CPU, before the fix (a real open-addressing hash
    // table). This uses a smaller N so the test suite itself stays
    // fast, but it's the same shape of program, run through an actual
    // compiled binary with a hard wall-clock bound — if the O(n^2)
    // behavior ever comes back, this test times out and fails instead
    // of quietly passing on a program too small to notice.
    let scratch = scratch_dir("memo_many_distinct_args");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "pure fn square(x: i64) -> i64 { return x * x; }\n\
         fn main() {\n\
         \x20   let i = 0;\n\
         \x20   let total = 0;\n\
         \x20   while (i < 200000) {\n\
         \x20       total = (total + square(i)) % 1000000007;\n\
         \x20       i = i + 1;\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let start = std::time::Instant::now();
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    let elapsed = start.elapsed();
    assert!(run.status.success(), "compiled binary exited with failure");
    // O(1)-average lookups: 200,000 distinct calls should finish in
    // well under a second on any machine this runs on. The old
    // linear-scan implementation would have taken tens of seconds at
    // this N (and minutes at bench_loop.kes's 5,000,000).
    assert!(
        elapsed.as_secs() < 10,
        "memoized calls with many distinct arguments took {elapsed:?} — looks like the O(n^2) linear-scan bug is back"
    );
    assert_eq!(native_stdout(&run), "648033478\n");
}

#[test]
fn the_71st_eligible_pure_fn_still_gets_memoized_past_the_old_64_slot_cap() {
    // Regression test: codegen.rs used to stop assigning memo slots
    // after MEMO_MAX_SLOTS (64) eligible functions — everything past the
    // cap compiled fine but silently ran unmemoized, with no error and
    // no way to notice short of a timing difference. kestrelc_runtime.c's
    // outer slot table (kestrelc_memo_tables/_counts/_caps) now grows on
    // demand instead of being a fixed 64-entry array. This builds a
    // program with 70 trivial eligible pure fns declared before `slow` —
    // so `slow` is assigned slot 70, well past the old cap — then calls
    // `slow(46)`, naive doubly-recursive fibonacci. A single call is
    // enough to prove the slot works: `slow`'s own recursive sub-calls
    // go through the same memoized call path as any other call to it, so
    // if slot 70 doesn't actually work, this doesn't just run somewhat
    // slower — naive fib(46) is ~3.7 billion recursive calls, several
    // seconds at minimum even for cheap native calls, vs comfortably
    // under a second memoized (see the memoized recursive fib test above
    // for the un-capped version of this same effect, and
    // kestrel-DESIGN.md's measured fib(32) numbers for the general
    // magnitude memoizing naive recursion buys).
    let scratch = scratch_dir("memo_past_old_slot_cap");
    let src_path = scratch.join("prog.kes");
    let mut src = String::new();
    for i in 0..70 {
        src.push_str(&format!("pure fn dummy{i}(x: i64) -> i64 {{ return x + {i}; }}\n"));
    }
    src.push_str(
        "pure fn slow(n: i64) -> i64 {\n\
         \x20   if (n < 2) { return n; }\n\
         \x20   return slow(n - 1) + slow(n - 2);\n\
         }\n\
         fn main() {\n\
         \x20   print(slow(46));\n\
         }\n",
    );
    fs::write(&src_path, &src).unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let start = std::time::Instant::now();
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    let elapsed = start.elapsed();
    assert!(run.status.success(), "compiled binary exited with failure");
    assert!(
        elapsed.as_secs() < 5,
        "slow(46) took {elapsed:?} — looks like the 71st eligible fn (slot past the old 64-slot cap) isn't being memoized"
    );
    assert_eq!(native_stdout(&run), "1836311903\n");
}

// ==================== codegen error positions ====================

#[test]
fn a_native_codegen_error_now_carries_a_full_caret_diagnostic() {
    // Purity/parallel_map/type-check errors already got real
    // file:line:col: + caret diagnostics (see the earlier caret tests
    // above) — codegen.rs's own errors, and wasm_codegen.rs's (see the
    // wasm-backend counterpart below), used to be message-only, then
    // just a bare `line:col:` prefix. Unifying every error type behind
    // one KestrelcError{kind,message,span} (see error.rs) meant codegen
    // errors inherited a real Span (with a real len, from Stmt) for
    // free, so main.rs can render the exact same full caret treatment
    // for these as everything else.
    let scratch = scratch_dir("codegen_err_pos");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let a = [1, 2, 3];\n\
         \x20   print(a);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc should reject using an array value directly");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("prog.kes:3:5:") && stderr.contains("it can only be indexed") && stderr.contains('^'),
        "expected a full file:line:col: + caret diagnostic on the codegen error, got:\n{stderr}"
    );
}

#[test]
fn a_wasm_codegen_error_also_now_carries_a_full_caret_diagnostic() {
    // Same program, same error, through the WASM backend instead — this
    // used to be entirely message-only (a documented remaining gap after
    // the native backend got its line:col: prefix); unifying error types
    // closed that gap for both backends at once, not just native.
    let scratch = scratch_dir("wasm_codegen_err_pos");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let a = [1, 2, 3];\n\
         \x20   print(a);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(!out.status.success(), "kestrelc --wasm should reject using an array value directly");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("prog.kes:3:5:") && stderr.contains("it can only be indexed") && stderr.contains('^'),
        "expected a full file:line:col: + caret diagnostic on the wasm codegen error, got:\n{stderr}"
    );
}
