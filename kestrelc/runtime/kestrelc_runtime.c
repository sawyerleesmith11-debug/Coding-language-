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
// at compile time by codegen.rs — see MemoState). No locking anywhere:
// codegen.rs only ever assigns a slot to a function that's never passed
// as parallel_map's callback argument (see inline.rs's
// collect_parallel_map_callbacks, reused for exactly this exclusion),
// so a memoized function's cache is provably only ever touched from the
// single calling thread — real safety from a compile-time exclusion,
// not a runtime lock nobody profiles this small would want to pay for.
//
// Real open-addressing hash table per slot (power-of-2 capacity, linear
// probing, grown at load factor 0.5), not the linear-scan array this
// used to be. The linear scan was fine for a handful of memoized calls
// but silently went quadratic — total cost O(n^2) — for a hot pure
// function called with many *distinct* arguments (a completely
// realistic case, not a contrived one): a benchmark calling a memoized
// function 5,000,000 times with a different argument each time hung for
// minutes, still burning CPU, no error, no timeout — worse than
// kestrel.js's memoization hitting a hard `Map` size limit and crashing
// cleanly. A hash table keeps each lookup/insert average O(1)
// regardless of how many distinct argument lists a function has seen.
//
// kestrelc used to cap eligible functions at KESTRELC_MEMO_MAX_SLOTS
// (64) via a fixed-size outer array indexed by slot — simplest thing
// that worked at first, but a program with more than 64 eligible pure
// fns just silently stopped memoizing past the cap, no error, easy to
// not notice. The outer table is now grown on demand instead (see
// kestrelc_memo_ensure_slot_capacity below), same "always an
// optimization, never a hard limit" posture the per-slot hash tables
// already have. Memoized functions are, by construction (see
// codegen.rs's eligibility check — excludes anything ever passed as a
// parallel_map callback), only ever called from the single thread that
// calls them directly, so growing the outer table needs no locking
// either, same as everything else here.
#define KESTRELC_MEMO_MAX_ARGS 4
#define KESTRELC_MEMO_INITIAL_CAP 16 // must be a power of 2
#define KESTRELC_MEMO_INITIAL_SLOT_CAPACITY 16

typedef struct {
    long long args[KESTRELC_MEMO_MAX_ARGS];
    int nargs;
    long long result;
    int occupied; // 0 = empty slot in the open-addressing table
} kestrelc_memo_entry;

static kestrelc_memo_entry** kestrelc_memo_tables = NULL; // kestrelc_memo_slot_capacity pointers, each NULL until first grown
static int* kestrelc_memo_counts = NULL; // occupied entries, one per slot
static int* kestrelc_memo_caps = NULL; // each slot's table capacity, always a power of 2 (0 = not yet allocated)
static int kestrelc_memo_slot_capacity = 0; // length of the three arrays above

// Grows the three outer arrays (by doubling) until `slot` is a valid
// index, zero-initializing every newly added slot so it reads as "never
// stored to" — same meaning a static zero-initialized array gave every
// slot for free before this became dynamic. Returns 0 (leaving the
// arrays at whatever size they already were) if an allocation fails —
// same "caching is always optional, never load-bearing" rule as
// everywhere else this cache touches the runtime; the caller (only
// kestrelc_memo_store — kestrelc_memo_lookup never needs to grow, a slot
// past the current capacity has definitely never been stored to) treats
// that as "skip caching this entry."
static int kestrelc_memo_ensure_slot_capacity(int slot) {
    if (slot < kestrelc_memo_slot_capacity) {
        return 1;
    }
    int new_capacity = kestrelc_memo_slot_capacity == 0 ? KESTRELC_MEMO_INITIAL_SLOT_CAPACITY : kestrelc_memo_slot_capacity * 2;
    while (new_capacity <= slot) {
        new_capacity *= 2;
    }
    kestrelc_memo_entry** new_tables = realloc(kestrelc_memo_tables, (size_t)new_capacity * sizeof(kestrelc_memo_entry*));
    if (!new_tables) {
        return 0;
    }
    kestrelc_memo_tables = new_tables;
    int* new_counts = realloc(kestrelc_memo_counts, (size_t)new_capacity * sizeof(int));
    if (!new_counts) {
        return 0;
    }
    kestrelc_memo_counts = new_counts;
    int* new_caps = realloc(kestrelc_memo_caps, (size_t)new_capacity * sizeof(int));
    if (!new_caps) {
        return 0;
    }
    kestrelc_memo_caps = new_caps;

    size_t added = (size_t)(new_capacity - kestrelc_memo_slot_capacity);
    memset(kestrelc_memo_tables + kestrelc_memo_slot_capacity, 0, added * sizeof(kestrelc_memo_entry*));
    memset(kestrelc_memo_counts + kestrelc_memo_slot_capacity, 0, added * sizeof(int));
    memset(kestrelc_memo_caps + kestrelc_memo_slot_capacity, 0, added * sizeof(int));
    kestrelc_memo_slot_capacity = new_capacity;
    return 1;
}

// FNV-1a over the raw argument bytes — not cryptographic, doesn't need
// to be; this is a cache key for a single process's own memoized calls,
// not a security boundary. Same algorithm as cache.rs's own fnv1a64 on
// the Rust side (independent implementations, same well-known constants
// — no shared code between the two, this is C linked into the compiled
// binary while that one runs inside kestrelc itself).
static unsigned long long kestrelc_memo_hash(const long long* args, int nargs) {
    unsigned long long h = 0xcbf29ce484222325ULL;
    const unsigned char* bytes = (const unsigned char*)args;
    size_t n = (size_t)nargs * sizeof(long long);
    for (size_t i = 0; i < n; i++) {
        h ^= bytes[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

static int kestrelc_memo_args_eq(const kestrelc_memo_entry* e, const long long* args, int nargs) {
    return e->nargs == nargs && memcmp(e->args, args, (size_t)nargs * sizeof(long long)) == 0;
}

// Inserts into `table` (capacity `cap`, a power of 2) via linear
// probing, for the initial fill and for rehashing into a grown table.
// Never called on an already-occupied key — callers only ever move
// distinct entries (a fresh miss's data, or another live entry being
// rehashed), so no duplicate-key case to handle here.
static void kestrelc_memo_raw_insert(kestrelc_memo_entry* table, int cap, const kestrelc_memo_entry* e) {
    unsigned long long h = kestrelc_memo_hash(e->args, e->nargs);
    int i = (int)(h & (unsigned long long)(cap - 1));
    while (table[i].occupied) {
        i = (i + 1) & (cap - 1);
    }
    table[i] = *e;
}

// Doubles a slot's table capacity and rehashes every live entry into
// it. A failed allocation here just means this slot's cache stops
// growing for the rest of the run (falls back to always missing once
// full, via the `occupied` check `kestrelc_memo_store` already does) —
// same "caching is always an optimization, never a correctness
// dependency" rule as everywhere else this cache touches the runtime.
static void kestrelc_memo_grow(int slot) {
    int old_cap = kestrelc_memo_caps[slot];
    kestrelc_memo_entry* old_table = kestrelc_memo_tables[slot];
    int new_cap = old_cap == 0 ? KESTRELC_MEMO_INITIAL_CAP : old_cap * 2;
    kestrelc_memo_entry* new_table = calloc((size_t)new_cap, sizeof(kestrelc_memo_entry));
    if (!new_table) {
        return;
    }
    for (int i = 0; i < old_cap; i++) {
        if (old_table[i].occupied) {
            kestrelc_memo_raw_insert(new_table, new_cap, &old_table[i]);
        }
    }
    free(old_table);
    kestrelc_memo_tables[slot] = new_table;
    kestrelc_memo_caps[slot] = new_cap;
}

// Returns 1 and writes the cached result to *out if this exact argument
// list was seen before; returns 0 (leaving *out untouched) on a miss.
int kestrelc_memo_lookup(int slot, const long long* args, int nargs, long long* out) {
    if (slot < 0 || slot >= kestrelc_memo_slot_capacity) {
        return 0; // never grown this far -> definitely never stored to
    }
    int cap = kestrelc_memo_caps[slot];
    if (cap == 0) {
        return 0; // table not yet allocated — nothing has ever been stored
    }
    kestrelc_memo_entry* table = kestrelc_memo_tables[slot];
    unsigned long long h = kestrelc_memo_hash(args, nargs);
    int i = (int)(h & (unsigned long long)(cap - 1));
    // Bounded by cap: every slot visited at most once, since insert
    // never lets the table fill completely (grown well before that —
    // see kestrelc_memo_store's load-factor check).
    for (int probes = 0; probes < cap; probes++) {
        if (!table[i].occupied) {
            return 0; // empty slot on the probe chain -> definitely not present
        }
        if (kestrelc_memo_args_eq(&table[i], args, nargs)) {
            *out = table[i].result;
            return 1;
        }
        i = (i + 1) & (cap - 1);
    }
    return 0;
}

// Called once per genuinely-computed (cache-miss) call, right before
// that call actually returns — see codegen.rs's memoized-function
// epilogue.
void kestrelc_memo_store(int slot, const long long* args, int nargs, long long result) {
    if (slot < 0 || nargs < 0 || nargs > KESTRELC_MEMO_MAX_ARGS) {
        return;
    }
    if (!kestrelc_memo_ensure_slot_capacity(slot)) {
        return; // allocation failure growing the outer table; skip caching this entry
    }
    // Grow before inserting whenever occupied would reach half of
    // capacity — keeps probe chains short (load factor <= 0.5) no
    // matter how many distinct argument lists a function accumulates.
    if (kestrelc_memo_caps[slot] == 0 || kestrelc_memo_counts[slot] * 2 >= kestrelc_memo_caps[slot]) {
        kestrelc_memo_grow(slot);
        if (kestrelc_memo_caps[slot] == 0) {
            return; // allocation failed in kestrelc_memo_grow; skip caching this entry
        }
    }
    kestrelc_memo_entry e;
    for (int i = 0; i < nargs; i++) {
        e.args[i] = args[i];
    }
    e.nargs = nargs;
    e.result = result;
    e.occupied = 1;
    kestrelc_memo_raw_insert(kestrelc_memo_tables[slot], kestrelc_memo_caps[slot], &e);
    kestrelc_memo_counts[slot]++;
}
