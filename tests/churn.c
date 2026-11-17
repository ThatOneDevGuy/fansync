// File-event generator for tests/event-burst-test.sh, as fast as it gets.
//   churn DIR SECONDS            rewrite DIR/f0..f4999 round-robin (logs, caches, databases)
//   churn DIR SECONDS unique     create DIR/f<n>, keeping only the last 100 (temp files)
//   churn DIR fill N             create N files in DIR/d<k>/ (a tree to sync)
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static void put(const char *p) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
        if (write(fd, "x", 1) < 0) perror(p);
        close(fd);
    }
}

int main(int argc, char **argv) {
    char p[4096];
    if (argc < 3) return 2;
    const char *dir = argv[1];
    mkdir(dir, 0755);
    if (!strcmp(argv[2], "fill") && argc > 3) {
        long n = atol(argv[3]);
        for (long i = 0; i < n; i++) {
            if (i % 1000 == 0) {
                snprintf(p, sizeof p, "%s/d%ld", dir, i / 1000);
                mkdir(p, 0755);
            }
            snprintf(p, sizeof p, "%s/d%ld/f%ld", dir, i / 1000, i);
            put(p);
        }
        return 0;
    }
    double secs = atof(argv[2]);
    int unique = argc > 3 && !strcmp(argv[3], "unique");
    struct timespec t0, t;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (long i = 0;; i++) {
        if ((i & 255) == 0) {
            clock_gettime(CLOCK_MONOTONIC, &t);
            if (t.tv_sec - t0.tv_sec + (t.tv_nsec - t0.tv_nsec) / 1e9 > secs) break;
        }
        snprintf(p, sizeof p, "%s/f%ld", dir, unique ? i : i % 5000);
        put(p);
        if (unique && i >= 100) {
            snprintf(p, sizeof p, "%s/f%ld", dir, i - 100);
            unlink(p);
        }
    }
    return 0;
}
