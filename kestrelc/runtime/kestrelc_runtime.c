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
