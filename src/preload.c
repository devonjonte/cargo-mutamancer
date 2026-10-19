/* Records what a test reads, for cargo-mutamancer: each file opened for
 * reading, or looked for and absent ("f PATH"), each directory listed ("d PATH"), and each file opened
 * to write ("w PATH": what the test itself made, whose later reads are its
 * own), one line each,
 * appended to the file MUTAMANCER_READS names. Preloaded into each test's
 * process (LD_PRELOAD); each call goes on to the real function. */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int log_fd = -2;

static void note(char kind, int dirfd, const char *path) {
    if (log_fd == -2) {
        const char *out = getenv("MUTAMANCER_READS");
        static int (*real_open)(const char *, int, ...);
        if (!real_open) real_open = dlsym(RTLD_NEXT, "open");
        log_fd = out ? real_open(out, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : -1;
    }
    if (log_fd < 0 || !path) return;
    char line[PATH_MAX * 2 + 8];
    char base[PATH_MAX];
    base[0] = 0;
    /* A path relative to a directory descriptor: that directory's path. */
    if (path[0] != '/' && dirfd != AT_FDCWD) {
        char link[64];
        snprintf(link, sizeof link, "/proc/self/fd/%d", dirfd);
        ssize_t n = readlink(link, base, sizeof base - 1);
        if (n > 0) base[n] = 0; else base[0] = 0;
    }
    int len = base[0] ? snprintf(line, sizeof line, "%c %s/%s\n", kind, base, path) : snprintf(line, sizeof line, "%c %s\n", kind, path);
    if (len > 0 && len < (int)sizeof line) {
        ssize_t w = write(log_fd, line, (size_t)len);
        (void)w;
    }
}

static int reads(int flags) { return (flags & O_ACCMODE) != O_WRONLY && !(flags & (O_CREAT | O_TRUNC)); }

#define OPEN_LIKE(name, ...)                                                   \
    int name(const char *path, int flags, ...) {                               \
        static int (*real)(const char *, int, ...);                            \
        if (!real) real = dlsym(RTLD_NEXT, #name);                             \
        mode_t mode = 0;                                                       \
        if (flags & (O_CREAT | O_TMPFILE)) {                                   \
            va_list ap;                                                        \
            va_start(ap, flags);                                               \
            mode = va_arg(ap, mode_t);                                         \
            va_end(ap);                                                        \
        }                                                                      \
        int fd = real(path, flags, mode);                                      \
        if (fd >= 0 || reads(flags)) note(reads(flags) ? ((flags & O_DIRECTORY) ? 'd' : 'f') : 'w', AT_FDCWD, path); \
        return fd;                                                             \
    }
OPEN_LIKE(open)
OPEN_LIKE(open64)

#define OPENAT_LIKE(name)                                                      \
    int name(int dirfd, const char *path, int flags, ...) {                    \
        static int (*real)(int, const char *, int, ...);                       \
        if (!real) real = dlsym(RTLD_NEXT, #name);                             \
        mode_t mode = 0;                                                       \
        if (flags & (O_CREAT | O_TMPFILE)) {                                   \
            va_list ap;                                                        \
            va_start(ap, flags);                                               \
            mode = va_arg(ap, mode_t);                                         \
            va_end(ap);                                                        \
        }                                                                      \
        int fd = real(dirfd, path, flags, mode);                               \
        if (fd >= 0 || reads(flags)) note(reads(flags) ? ((flags & O_DIRECTORY) ? 'd' : 'f') : 'w', dirfd, path); \
        return fd;                                                             \
    }
OPENAT_LIKE(openat)
OPENAT_LIKE(openat64)

FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen");
    FILE *f = real(path, mode);
    if (mode && (f || mode[0] == 'r')) note(mode[0] == 'r' && !strchr(mode, '+') ? 'f' : 'w', AT_FDCWD, path);
    return f;
}

DIR *opendir(const char *path) {
    static DIR *(*real)(const char *);
    if (!real) real = dlsym(RTLD_NEXT, "opendir");
    DIR *d = real(path);
    note('d', AT_FDCWD, path);
    return d;
}
