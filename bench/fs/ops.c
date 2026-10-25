#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
// Per-operation latency of what unzip does per file, from the guest
// (bench/fs/parallel-creates.sh): ops <dir> [threads], 2000 files a thread.
enum { CREATE, WRITE, CLOSE, CHMOD, UTIME, STAT, MKDIR, NOPS };
static const char *names[] = {"create", "write", "close", "chmod", "utime", "stat", "mkdir"};
static double tot[64][NOPS]; static int N = 2000; static const char *base;
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e6 + t.tv_nsec / 1e3; }
static void *run(void *arg) {
  long id = (long)arg; char d[512], p[600]; char buf[1024]; memset(buf, 'x', sizeof buf);
  snprintf(d, sizeof d, "%s/t%ld", base, id); double t = now(); mkdir(d, 0755); tot[id][MKDIR] += now() - t;
  for (int i = 0; i < N; i++) {
    if (i % 50 == 0) { snprintf(d, sizeof d, "%s/t%ld/d%d", base, id, i / 50); t = now(); mkdir(d, 0755); tot[id][MKDIR] += now() - t; }
    snprintf(p, sizeof p, "%s/f%d.php", d, i);
    t = now(); int fd = open(p, O_CREAT | O_EXCL | O_WRONLY, 0644); tot[id][CREATE] += now() - t;
    t = now(); write(fd, buf, sizeof buf); tot[id][WRITE] += now() - t;
    t = now(); close(fd); tot[id][CLOSE] += now() - t;
    t = now(); chmod(p, 0644); tot[id][CHMOD] += now() - t;
    struct timespec ts[2] = {{1700000000, 0}, {1700000000, 0}};
    t = now(); utimensat(AT_FDCWD, p, ts, 0); tot[id][UTIME] += now() - t;
    struct stat st; t = now(); stat(p, &st); tot[id][STAT] += now() - t;
  }
  return 0;
}
int main(int argc, char **argv) {
  base = argv[1]; int th = argc > 2 ? atoi(argv[2]) : 1; pthread_t t[64];
  double s = now();
  for (long i = 0; i < th; i++) pthread_create(&t[i], 0, run, (void *)i);
  for (int i = 0; i < th; i++) pthread_join(t[i], 0);
  double wall = now() - s;
  printf("threads %d, %d files each, %.2f s, per file µs:", th, N, wall / 1e6);
  for (int o = 0; o < NOPS; o++) { double a = 0; for (int i = 0; i < th; i++) a += tot[i][o]; printf(" %s %.1f", names[o], a / th / (o == MKDIR ? N / 50 + 1 : N)); }
  printf("\n");
}
