/* the few POSIX calls Rust's std (newlib/horizon path) links that devkitPPC's
   newlib and wut leave out. */
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <unistd.h>
#include <coreinit/time.h>
#include <coreinit/thread.h>

long sysconf(int name) {
    switch (name) {
    case _SC_PAGESIZE: return 4096;
#ifdef _SC_NPROCESSORS_ONLN
    case _SC_NPROCESSORS_ONLN: return 3;
#endif
#ifdef _SC_NPROCESSORS_CONF
    case _SC_NPROCESSORS_CONF: return 3;
#endif
#ifdef _SC_OPEN_MAX
    case _SC_OPEN_MAX: return 64;
#endif
    default: errno = EINVAL; return -1;
    }
}

int chown(const char *path, uid_t owner, gid_t group) { (void)path; (void)owner; (void)group; errno = ENOSYS; return -1; }
int lchown(const char *path, uid_t owner, gid_t group) { (void)path; (void)owner; (void)group; errno = ENOSYS; return -1; }
int chroot(const char *path) { (void)path; errno = ENOSYS; return -1; }
int mkfifo(const char *path, mode_t mode) { (void)path; (void)mode; errno = ENOSYS; return -1; }

/* the Wii U has no entropy source a title can read simply, and std only
   wants this for HashMap keys: a splitmix64 stream seeded from the clock and
   the thread. */
ssize_t getrandom(void *buffer, size_t length, unsigned int flags) {
    static uint64_t state;
    (void)flags;
    if (!state) state = (uint64_t)OSGetTime() ^ ((uint64_t)(uintptr_t)OSGetCurrentThread() << 32) ^ 0x9E3779B97F4A7C15ull;
    uint8_t *out = buffer;
    for (size_t i = 0; i < length; i++) {
        if ((i & 7) == 0) {
            state += 0x9E3779B97F4A7C15ull;
            state ^= (uint64_t)OSGetSystemTick();
        }
        uint64_t z = state + i;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
        out[i] = (uint8_t)((z ^ (z >> 31)) >> ((i & 7) * 8));
    }
    return (ssize_t)length;
}

/* ---- pthread keys ----
   devkitPPC's pthread_key_create/getspecific/setspecific forward to the
   weak __syscall_tls_* hooks, which wut leaves out, so without these keys
   fail with ENOSYS. Rust's std (has-thread-local false on this target) keeps
   every thread_local! behind a pthread key, and aborts when it can't make
   one.

   Each thread's values live in an array hung off one of coreinit's
   per-thread slots (13; wut keeps 14 and 15). Every key carries a
   generation, bumped when it's deleted, so a reused key reads NULL on
   threads that set the old one, as POSIX wants. Destructors run when a
   thread ends, from a cleanup callback chained in front of whichever the
   thread had (wut's reent cleanup chains the same way). */
#include <stdlib.h>
#include <coreinit/spinlock.h>
#include <coreinit/thread.h>

#define ZW_TLS_KEYS 256
#define ZW_TLS_SLOT OS_THREAD_SPECIFIC_13
/* POSIX's PTHREAD_DESTRUCTOR_ITERATIONS */
#define ZW_TLS_DESTRUCTOR_PASSES 4

typedef struct TlsThread {
    OSThreadCleanupCallbackFn previous;
    void *values[ZW_TLS_KEYS];
    uint32_t generations[ZW_TLS_KEYS];
} TlsThread;

static OSSpinLock tls_lock;

/* a zeroed OSSpinLock is already an unlocked one; this makes it explicit,
   before main, if the startup code runs constructors */
__attribute__((constructor)) static void tls_init(void) {
    OSInitSpinLock(&tls_lock);
}
static uint8_t tls_used[ZW_TLS_KEYS];
static uint32_t tls_generation[ZW_TLS_KEYS];
static void (*tls_destructors[ZW_TLS_KEYS])(void *);

static void tls_acquire(void) {
    OSUninterruptibleSpinLock_Acquire(&tls_lock);
}

static void tls_release(void) {
    OSUninterruptibleSpinLock_Release(&tls_lock);
}

static void tls_cleanup(OSThread *thread, void *stack) {
    TlsThread *tls = (TlsThread *)OSGetThreadSpecific(ZW_TLS_SLOT);
    OSThreadCleanupCallbackFn previous = tls ? tls->previous : NULL;
    if (tls) {
        for (int pass = 0; pass < ZW_TLS_DESTRUCTOR_PASSES; pass++) {
            int ran = 0;
            for (uint32_t k = 1; k < ZW_TLS_KEYS; k++) {
                void *value = tls->values[k];
                if (!value) continue;
                tls_acquire();
                void (*destructor)(void *) =
                    (tls_used[k] && tls->generations[k] == tls_generation[k]) ? tls_destructors[k] : NULL;
                tls_release();
                tls->values[k] = NULL;
                if (destructor) {
                    destructor(value);
                    ran = 1;
                }
            }
            if (!ran) break;
        }
        OSSetThreadSpecific(ZW_TLS_SLOT, NULL);
        free(tls);
    }
    if (previous) previous(thread, stack);
}

static TlsThread *tls_thread(int create) {
    TlsThread *tls = (TlsThread *)OSGetThreadSpecific(ZW_TLS_SLOT);
    if (!tls && create) {
        tls = calloc(1, sizeof *tls);
        if (tls) {
            OSSetThreadSpecific(ZW_TLS_SLOT, tls);
            tls->previous = OSSetThreadCleanupCallback(OSGetCurrentThread(), tls_cleanup);
        }
    }
    return tls;
}

int __syscall_tls_create(uint32_t *key, void (*destructor)(void *)) {
    tls_acquire();
    /* key 0 is left out, some code takes 0 for "no key" */
    for (uint32_t k = 1; k < ZW_TLS_KEYS; k++) {
        if (!tls_used[k]) {
            tls_used[k] = 1;
            tls_destructors[k] = destructor;
            tls_release();
            *key = k;
            return 0;
        }
    }
    tls_release();
    return EAGAIN;
}

int __syscall_tls_delete(uint32_t key) {
    if (key == 0 || key >= ZW_TLS_KEYS) return EINVAL;
    tls_acquire();
    tls_used[key] = 0;
    tls_destructors[key] = NULL;
    tls_generation[key]++;
    tls_release();
    return 0;
}

void *__syscall_tls_get(uint32_t key) {
    if (key == 0 || key >= ZW_TLS_KEYS) return NULL;
    TlsThread *tls = tls_thread(0);
    if (!tls) return NULL;
    /* a value set under a deleted key's generation is gone; generation
       reads are word-sized and only change on delete */
    return tls->generations[key] == tls_generation[key] ? tls->values[key] : NULL;
}

int __syscall_tls_set(uint32_t key, const void *value) {
    if (key == 0 || key >= ZW_TLS_KEYS) return EINVAL;
    TlsThread *tls = tls_thread(1);
    if (!tls) return ENOMEM;
    tls->values[key] = (void *)value;
    tls->generations[key] = tls_generation[key];
    return 0;
}
