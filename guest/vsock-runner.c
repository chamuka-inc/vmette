// vsock-runner — guest-side AF_VSOCK command server for vmette snapshots.
//
// Flow:
//   1. Connect to CID=host, port=SIGNAL_PORT and send "READY\n".  This is
//      the host's signal to pause the VM and write a snapshot.
//   2. Open a listener on CID=any, port=LISTEN_PORT.  accept() blocks until
//      the host (post-restore) connects.
//   3. Read the command from the connection (peer half-closes when done).
//   4. fork + exec /bin/sh -c <cmd>, streaming stdout+stderr back through
//      the same connection.
//   5. Write "__EXIT__:<N>\n" with the child's exit code, close, reboot.
//
// Snapshot semantics: VZ pauses the guest somewhere between (1) and (4).
// On the first invocation the snapshot is taken right after READY; on each
// resume, vsock-runner continues from accept() — so each resume serves
// exactly one command.
//
// Build (host, macOS Intel):
//   x86_64-linux-musl-gcc -static -O2 -s -o vsock-runner vsock-runner.c

#define _GNU_SOURCE
#include <sys/socket.h>
#include <sys/wait.h>
#include <sys/reboot.h>
#include <linux/vm_sockets.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>

static int connect_host(unsigned port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_vm a;
    memset(&a, 0, sizeof(a));
    a.svm_family = AF_VSOCK;
    a.svm_cid    = VMADDR_CID_HOST;
    a.svm_port   = port;
    if (connect(fd, (struct sockaddr *)&a, sizeof(a)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int listen_guest(unsigned port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_vm a;
    memset(&a, 0, sizeof(a));
    a.svm_family = AF_VSOCK;
    a.svm_cid    = VMADDR_CID_ANY;
    a.svm_port   = port;
    if (bind(fd, (struct sockaddr *)&a, sizeof(a)) < 0) {
        close(fd);
        return -1;
    }
    if (listen(fd, 1) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void write_all(int fd, const void *buf, size_t n) {
    const char *p = (const char *)buf;
    while (n > 0) {
        ssize_t w = write(fd, p, n);
        if (w < 0) { if (errno == EINTR) continue; return; }
        p += w; n -= (size_t)w;
    }
}

static int run_and_stream(int conn_fd, const char *cmd) {
    int pipefd[2];
    if (pipe(pipefd) < 0) { perror("pipe"); return 127; }

    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 127; }

    if (pid == 0) {
        // child: stdout + stderr → pipe write end
        dup2(pipefd[1], STDOUT_FILENO);
        dup2(pipefd[1], STDERR_FILENO);
        close(pipefd[0]);
        close(pipefd[1]);
        close(conn_fd);
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }

    close(pipefd[1]);
    char buf[4096];
    ssize_t n;
    while ((n = read(pipefd[0], buf, sizeof(buf))) > 0) {
        write_all(conn_fd, buf, (size_t)n);
    }
    close(pipefd[0]);

    int st = 0;
    while (waitpid(pid, &st, 0) < 0 && errno == EINTR) {}
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s SIGNAL_PORT LISTEN_PORT\n", argv[0]);
        return 2;
    }
    unsigned sigp = (unsigned)strtoul(argv[1], NULL, 10);
    unsigned lstp = (unsigned)strtoul(argv[2], NULL, 10);

    // 1. Tell the host we're ready to be snapshotted.
    int s = connect_host(sigp);
    if (s < 0) {
        fprintf(stderr, "vsock-runner: connect to host:%u failed: %s\n",
                sigp, strerror(errno));
        return 1;
    }
    write_all(s, "READY\n", 6);
    close(s);

    // 2. Wait for the host to push a command. The snapshot will pause us
    //    inside accept(); on resume we continue here.
    int l = listen_guest(lstp);
    if (l < 0) {
        fprintf(stderr, "vsock-runner: listen on :%u failed: %s\n",
                lstp, strerror(errno));
        return 1;
    }

    struct sockaddr_vm peer;
    socklen_t plen = sizeof(peer);
    int c;
    while ((c = accept(l, (struct sockaddr *)&peer, &plen)) < 0) {
        if (errno != EINTR) {
            perror("accept");
            return 1;
        }
    }
    close(l);

    // 3. Read the command (up to 64 KiB, peer half-closes when done).
    char cmd[65536];
    size_t off = 0;
    while (off < sizeof(cmd) - 1) {
        ssize_t n = read(c, cmd + off, sizeof(cmd) - 1 - off);
        if (n < 0) { if (errno == EINTR) continue; break; }
        if (n == 0) break;
        off += (size_t)n;
    }
    cmd[off] = '\0';

    // 4. Run + stream output back.
    int rc = run_and_stream(c, cmd);

    // 5. Send exit code marker + half-close.
    char tail[64];
    int tl = snprintf(tail, sizeof(tail), "__EXIT__:%d\n", rc);
    write_all(c, tail, (size_t)tl);
    shutdown(c, SHUT_WR);

    // Drain anything remaining so the peer sees us done.
    char b2[4096];
    while (read(c, b2, sizeof(b2)) > 0) {}
    close(c);

    sync();
    reboot(RB_POWER_OFF);
    // reboot returns on failure; spin so kernel doesn't see us exit as PID 1.
    for (;;) pause();
    return 0;
}
