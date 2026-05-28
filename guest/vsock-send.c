// vsock-send — minimal AF_VSOCK SOCK_STREAM client for the vmette guest.
//
// Pipes stdin into a vsock connection to the host, then drains the reply
// back to stdout until the peer closes. busybox `nc` doesn't speak AF_VSOCK
// so this fills that gap.
//
// Build (host, macOS Intel):
//   x86_64-linux-musl-gcc -static -O2 -s -o vsock-send vsock-send.c
//
// Usage (inside the vmette guest):
//   echo "hello" | vsock-send 1024            # CID defaults to 2 (host)
//   vsock-send 1024 < some-file
//
// CID conventions:
//   0  reserved (hypervisor)    1  loopback
//   2  host                     3+ guests

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>

static void usage(const char *prog) {
    fprintf(stderr,
        "usage: %s PORT [CID]\n"
        "  PORT  AF_VSOCK port on the destination\n"
        "  CID   destination CID (default 2 — the host)\n"
        "  reads stdin, sends to vsock CID:PORT, then prints any reply to stdout\n",
        prog);
}

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) { usage(argv[0]); return 2; }

    unsigned port = (unsigned)strtoul(argv[1], NULL, 10);
    unsigned cid  = (argc >= 3) ? (unsigned)strtoul(argv[2], NULL, 10) : VMADDR_CID_HOST;

    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket(AF_VSOCK)"); return 1; }

    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid    = cid;
    addr.svm_port   = port;

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        fprintf(stderr, "connect to vsock %u:%u failed: %s\n",
                cid, port, strerror(errno));
        return 1;
    }

    // stdin → socket
    char buf[4096];
    ssize_t n;
    while ((n = read(0, buf, sizeof(buf))) > 0) {
        ssize_t off = 0;
        while (off < n) {
            ssize_t w = write(fd, buf + off, (size_t)(n - off));
            if (w < 0) {
                if (errno == EINTR) continue;
                perror("write(vsock)");
                return 1;
            }
            off += w;
        }
    }
    if (n < 0) { perror("read(stdin)"); return 1; }

    // Half-close so the host sees EOF on its read side.
    shutdown(fd, SHUT_WR);

    // socket → stdout (until peer closes)
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        ssize_t off = 0;
        while (off < n) {
            ssize_t w = write(1, buf + off, (size_t)(n - off));
            if (w < 0) {
                if (errno == EINTR) continue;
                perror("write(stdout)");
                return 1;
            }
            off += w;
        }
    }

    close(fd);
    return 0;
}
