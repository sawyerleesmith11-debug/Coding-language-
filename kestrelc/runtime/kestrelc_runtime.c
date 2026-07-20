// kestrelc's native-backend runtime support — currently just one
// function: real OS-thread parallelism for `parallel_map(f, arr)` (see
// kestrel-DESIGN.md idea #5, "fearless parallelism, powered by
// purity"). Compiled and linked alongside every kestrelc-produced object
// file (see the `link_and_report` step in kestrelc/src/main.rs) whether
// or not a given program actually uses parallel_map — it's a handful of
// instructions or a no-op otherwise, not worth a second linker pass to
// avoid.
//
// Why a C shim instead of hand-written Cranelift IR: Cranelift has no
// pthread-aware primitives of its own, and hand-rolling pthread_create
// calls in raw IR would mean re-deriving the System V ABI's struct
// layouts and register conventions by hand for zero benefit — `cc`
// already knows how to compile straightforward C and link it against
// libpthread. Kestrelc's generated code just calls this one function,
// the same way it already calls libc's `printf`.

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <unistd.h>
#endif

// sysconf(_SC_NPROCESSORS_ONLN) is a POSIX/glibc extension MinGW-w64's
// UCRT doesn't implement — Windows' own answer to "how many logical
// processors" is GetSystemInfo. Both return the same kind of number
// (logical processor count, no P-core/E-core distinction), so the
// len<10000-or-single-core fallback heuristic below behaves identically
// either way.
static long kestrelc_nprocs(void) {
#ifdef _WIN32
    SYSTEM_INFO info;
    GetSystemInfo(&info);
    return (long)info.dwNumberOfProcessors;
#else
    return sysconf(_SC_NPROCESSORS_ONLN);
#endif
}

typedef struct {
    const long long* in;
    long long* out;
    long long start;
    long long end;
    long long (*f)(long long);
} kestrelc_pmap_chunk;

static void* kestrelc_pmap_worker(void* arg) {
    kestrelc_pmap_chunk* c = (kestrelc_pmap_chunk*)arg;
    for (long long i = c->start; i < c->end; i++) {
        c->out[i] = c->f(c->in[i]);
    }
    return NULL;
}

// Called by kestrelc-generated code for `parallel_map(f, arr)`: writes
// `f(in[i])` into `out[i]` for every `i` in `[0, len)`. Below a fixed
// size threshold (thread setup/teardown would cost more than it saves)
// or on a single-core machine, runs inline on the calling thread instead
// — a real, if simple, heuristic for "is splitting this actually worth
// it," which is exactly the trade-off kestrel-DESIGN.md's idea #5 names
// as real additional engineering, not a free unlock.
void kestrelc_parallel_map_i64(const long long* in, long long len, long long (*f)(long long), long long* out) {
    if (len <= 0) {
        return;
    }

    long nprocs = kestrelc_nprocs();
    if (nprocs < 1) {
        nprocs = 1;
    }
    if (nprocs > len) {
        nprocs = len;
    }

    if (len < 10000 || nprocs <= 1) {
        for (long long i = 0; i < len; i++) {
            out[i] = f(in[i]);
        }
        return;
    }

    pthread_t* threads = malloc(sizeof(pthread_t) * (size_t)nprocs);
    kestrelc_pmap_chunk* chunks = malloc(sizeof(kestrelc_pmap_chunk) * (size_t)nprocs);
    if (!threads || !chunks) {
        // Allocation failure: fall back to running inline rather than
        // crashing — this is a performance path, never a correctness one.
        free(threads);
        free(chunks);
        for (long long i = 0; i < len; i++) {
            out[i] = f(in[i]);
        }
        return;
    }

    long long chunk_size = len / nprocs;
    long long start = 0;
    for (long t = 0; t < nprocs; t++) {
        long long end = (t == nprocs - 1) ? len : start + chunk_size;
        chunks[t].in = in;
        chunks[t].out = out;
        chunks[t].start = start;
        chunks[t].end = end;
        chunks[t].f = f;
        pthread_create(&threads[t], NULL, kestrelc_pmap_worker, &chunks[t]);
        start = end;
    }
    for (long t = 0; t < nprocs; t++) {
        pthread_join(threads[t], NULL);
    }

    free(threads);
    free(chunks);
}

// Called by kestrelc-generated code for an *unprovable* array access
// (no `where` clause covers it, so it's checked at runtime — see
// codegen.rs's Expr::Index arm) once the bounds check actually fails.
// Previously this was a bare trap: the process died with SIGILL and no
// indication of what went wrong, unlike run()/runFast(), which print a
// message and exit cleanly. Prints the same kind of message those
// backends do, then exits with a real error status instead of
// crashing. Declared to never return so the one instruction generated
// after calling it (still a trap, to satisfy Cranelift's "every block
// needs a terminator" rule) is unreachable in practice, not a real
// fallback path.
void kestrelc_bounds_fail(long long idx, long long len) {
    fprintf(stderr, "kestrelc: Index %lld out of bounds for array of length %lld\n", idx, len);
    exit(1);
}

// Small in-memory table of a *previous* run's recorded counts, loaded
// once (on the first kestrelc_profile_record call of this process) so
// every subsequent call in the same flush sequence can look up "what did
// we already know about this function." Kestrel programs compiled by
// kestrelc so far are small (a handful of functions), so a fixed-size
// linear-scan table is plenty — this isn't meant to scale to a large
// program, just to avoid a second file format/library dependency for
// something this size.
#define KESTRELC_PROFILE_MAX_ENTRIES 512
typedef struct { char* name; long long len; long long count; } kestrelc_profile_entry;
static kestrelc_profile_entry kestrelc_profile_prev[KESTRELC_PROFILE_MAX_ENTRIES];
static int kestrelc_profile_prev_n = 0;
static int kestrelc_profile_prev_loaded = 0;

static void kestrelc_profile_load_prev(const char* path) {
    kestrelc_profile_prev_loaded = 1;
    FILE* f = fopen(path, "r");
    if (!f) {
        return;
    }
    char line[1024];
    while (kestrelc_profile_prev_n < KESTRELC_PROFILE_MAX_ENTRIES && fgets(line, sizeof(line), f)) {
        char* sp = strrchr(line, ' ');
        if (!sp) {
            continue;
        }
        *sp = 0;
        long long count = atoll(sp + 1);
        long long len = (long long)strlen(line);
        char* copy = malloc((size_t)len + 1);
        if (!copy) {
            break;
        }
        memcpy(copy, line, (size_t)len + 1);
        kestrelc_profile_entry* e = &kestrelc_profile_prev[kestrelc_profile_prev_n++];
        e->name = copy;
        e->len = len;
        e->count = count;
    }
    fclose(f);
}

static long long kestrelc_profile_prev_count(const char* name, long long name_len) {
    for (int i = 0; i < kestrelc_profile_prev_n; i++) {
        if (kestrelc_profile_prev[i].len == name_len && memcmp(kestrelc_profile_prev[i].name, name, (size_t)name_len) == 0) {
            return kestrelc_profile_prev[i].count;
        }
    }
    return 0;
}

// Appends one "<name> <count>\n" line to the profile file kestrelc
// embedded as data in this program's own object file (see codegen.rs's
// ProfileState) — one call per function, emitted in `main`'s epilogue
// once every explicit `return` (and any implicit fall-off-the-end) has
// been redirected there instead of returning directly. Not called at
// all for wasm or when no compile cache directory is available (see
// codegen.rs) — this file only exists to feed the *next* `kestrelc`
// invocation's inlining pass (see inline.rs), never anything the
// running program itself reads.
//
// Written count is `max(this run's count, the previous run's recorded
// count)`, not just this run's raw count — critical for the feedback
// loop to actually settle instead of oscillating forever: once a
// function is inlined at its call sites (because a previous run showed
// it was hot), the *next* run's compiled binary no longer contains any
// real calls to it, so its own counter would read back as 0. Recording
// a raw 0 would make the compiler after *that* run conclude "not hot
// anymore" and un-inline it — which makes it hot again next run — an
// infinite flip-flop. Keeping the historical high-water mark instead
// means "was ever called this often" stays true once observed, so a
// function that's been proven worth inlining stays inlined.
//
// Byte counts, not null-terminated C strings, since both the path and
// the function name are raw pointers into this object's own read-only
// data section — safe to bound with an explicit length, no assumption
// about what follows them in memory.
void kestrelc_profile_record(const char* path, long long path_len, const char* name, long long name_len, long long count, int is_first) {
    if (path_len < 0 || path_len >= 4096 || name_len < 0) {
        return;
    }
    char pbuf[4096];
    memcpy(pbuf, path, (size_t)path_len);
    pbuf[path_len] = 0;

    if (!kestrelc_profile_prev_loaded) {
        kestrelc_profile_load_prev(pbuf);
    }
    long long prev = kestrelc_profile_prev_count(name, name_len);
    long long merged = count > prev ? count : prev;

    FILE* f = fopen(pbuf, is_first ? "w" : "a");
    if (!f) {
        return;
    }
    fwrite(name, 1, (size_t)name_len, f);
    fprintf(f, " %lld\n", merged);
    fclose(f);
}

// Memoization cache for `pure fn` calls — kestrel-DESIGN.md idea #2/#4:
// a pure function can't observe or be affected by any other call to
// itself, so caching by argument value is always safe. `run`/`runFast`
// (kestrel.js) already do this per-interpreter-run; this is the native
// backend's version. One table per memoized function ("slot", assigned
// at compile time by codegen.rs — see MemoState), each table a plain
// growable array scanned linearly. No locking anywhere: codegen.rs only
// ever assigns a slot to a function that's never passed as
// parallel_map's callback argument (see inline.rs's
// collect_parallel_map_callbacks, reused for exactly this exclusion),
// so a memoized function's cache is provably only ever touched from the
// single calling thread — real safety from a compile-time exclusion,
// not a runtime lock nobody profiles this small would want to pay for.
//
// Fixed-size slot table, not a dynamic map: kestrelc assigns slots
// sequentially per compiled program and never exceeds
// KESTRELC_MEMO_MAX_SLOTS (see codegen.rs) — a plain array indexed by
// slot is simplest and fastest for a compiler this scoped.
#define KESTRELC_MEMO_MAX_SLOTS 64
#define KESTRELC_MEMO_MAX_ARGS 4

typedef struct {
    long long args[KESTRELC_MEMO_MAX_ARGS];
    int nargs;
    long long result;
} kestrelc_memo_entry;

static kestrelc_memo_entry* kestrelc_memo_tables[KESTRELC_MEMO_MAX_SLOTS];
static int kestrelc_memo_counts[KESTRELC_MEMO_MAX_SLOTS];
static int kestrelc_memo_caps[KESTRELC_MEMO_MAX_SLOTS];

// Returns 1 and writes the cached result to *out if this exact argument
// list was seen before; returns 0 (leaving *out untouched) on a miss.
int kestrelc_memo_lookup(int slot, const long long* args, int nargs, long long* out) {
    if (slot < 0 || slot >= KESTRELC_MEMO_MAX_SLOTS) {
        return 0;
    }
    kestrelc_memo_entry* table = kestrelc_memo_tables[slot];
    int n = kestrelc_memo_counts[slot];
    for (int i = 0; i < n; i++) {
        if (table[i].nargs == nargs && memcmp(table[i].args, args, (size_t)nargs * sizeof(long long)) == 0) {
            *out = table[i].result;
            return 1;
        }
    }
    return 0;
}

// Called once per genuinely-computed (cache-miss) call, right before
// that call actually returns — see codegen.rs's memoized-function
// epilogue. A realloc failure here just means this one entry never gets
// cached (the function still returns the correct value either way) —
// same "caching is always an optimization, never a correctness
// dependency" rule as cache.rs and profile.rs.
void kestrelc_memo_store(int slot, const long long* args, int nargs, long long result) {
    if (slot < 0 || slot >= KESTRELC_MEMO_MAX_SLOTS || nargs < 0 || nargs > KESTRELC_MEMO_MAX_ARGS) {
        return;
    }
    if (kestrelc_memo_counts[slot] >= kestrelc_memo_caps[slot]) {
        int new_cap = kestrelc_memo_caps[slot] == 0 ? 16 : kestrelc_memo_caps[slot] * 2;
        kestrelc_memo_entry* grown = realloc(kestrelc_memo_tables[slot], (size_t)new_cap * sizeof(kestrelc_memo_entry));
        if (!grown) {
            return;
        }
        kestrelc_memo_tables[slot] = grown;
        kestrelc_memo_caps[slot] = new_cap;
    }
    kestrelc_memo_entry* e = &kestrelc_memo_tables[slot][kestrelc_memo_counts[slot]++];
    for (int i = 0; i < nargs; i++) {
        e->args[i] = args[i];
    }
    e->nargs = nargs;
    e->result = result;
}
