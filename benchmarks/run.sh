#!/bin/bash
# Rebuild and time all 5 benchmark workloads. Run from the benchmarks/
# directory. Requires cc (mingw/gcc) and rustc on PATH, and
# kestrelc/target/release/kestrelc.exe already built (--features native).
set -e

export PATH="/c/Users/sawye/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin:$PATH"
export USERPROFILE="${USERPROFILE:-C:\Users\sawye}"
unset HOME
# Absolute path, resolved once before any `cd` below -- a path relative
# to this script's own directory (benchmarks/) breaks once the loop
# below `cd`s one level deeper into each workload's own subdirectory.
KESTRELC="$(pwd)/../kestrelc/target/release/kestrelc.exe"
TIMEFORMAT='%R'

median5() {
    local bin="$1"
    local times=()
    for i in 1 2 3 4 5; do
        local t
        t=$( { time "$bin" >/dev/null; } 2>&1 )
        times+=("$t")
    done
    printf '%s\n' "${times[@]}" | sort -n | sed -n '3p'
}

for dir in integer-loop fib-recursive array-sum parallel-map bounds-heavy; do
    echo "=== $dir ==="
    cd "$dir"
    cc -O2 bench.c -o bench_c_o2
    cc -O3 -march=native bench.c -o bench_c_o3
    # -C opt-level=3 is rustc's equivalent of a --release build's default
    # optimization level -- matches the "fair comparison" intent of the
    # two C variants above (-O2 and -O3 -march=native).
    # Explicit .exe suffix: unlike mingw cc above (which appends it
    # automatically on Windows even without being asked), rustc in this
    # environment doesn't -- and this repo's .gitignore only ignores
    # *.exe, not a bare extensionless binary name.
    rustc -C opt-level=3 bench.rs -o bench_rust.exe 2>/dev/null
    "$KESTRELC" bench.kes
    # warm run for kestrelc's profile-guided inlining/memoization, then
    # recompile so the warmed profile is actually reflected in codegen
    ./bench >/dev/null
    "$KESTRELC" bench.kes >/dev/null

    out_k=$(./bench)
    out_o2=$(./bench_c_o2)
    out_o3=$(./bench_c_o3)
    out_rust=$(./bench_rust.exe)
    if [ "$out_k" != "$out_o2" ] || [ "$out_k" != "$out_o3" ] || [ "$out_k" != "$out_rust" ]; then
        echo "MISMATCH: kestrel=$out_k c-o2=$out_o2 c-o3=$out_o3 rust=$out_rust"
        cd ..
        continue
    fi

    t_k=$(median5 ./bench)
    t_o2=$(median5 ./bench_c_o2)
    t_o3=$(median5 ./bench_c_o3)
    t_rust=$(median5 ./bench_rust.exe)
    echo "kestrel=${t_k}s  c-o2=${t_o2}s  c-o3=${t_o3}s  rust=${t_rust}s  (output: $out_k)"
    cd ..
done
