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
