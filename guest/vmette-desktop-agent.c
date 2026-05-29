// vmette-desktop-agent — in-guest computer-use agent for vmette's desktop
// (Agent) workload. Drives a headless Xvfb display via XTEST (synthetic
// input) and XGetImage (framebuffer capture), speaking vmette's framed
// protocol over AF_VSOCK to the host vmette::Session.
//
// Unlike the static musl initramfs helpers (vsock-send / vsock-runner),
// this binary links libX11 + libXtst dynamically and therefore lives
// *inside* the desktop rootfs image, compiled against that image's libc.
//
// Wire protocol (must match crates/vmette/src/desktop.rs):
//   request  (host → guest): [u32 LE header_len][header JSON]          (no payload)
//   response (guest → host): [u32 LE header_len][header JSON][payload]  (payload optional)
// The response header carries "payload_len" = number of trailing bytes
// (a PNG for screenshots; 0 otherwise).
//
// Flow:
//   1. Open the X display (default ":99").
//   2. connect() to CID=host, the port given on argv (the host's listener).
//   3. Serve framed requests until the connection closes, then exit (the
//      desktop entrypoint decides whether to relaunch / power off).
//
// Build: see scripts/build-desktop-agent.sh (compiled inside the image).
//   cc -O2 -o vmette-desktop-agent vmette-desktop-agent.c -lX11 -lXtst

#define _GNU_SOURCE
#include <sys/socket.h>
#include <linux/vm_sockets.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/keysym.h>
#include <X11/extensions/XTest.h>

#define STB_IMAGE_WRITE_IMPLEMENTATION
#define STBI_WRITE_NO_STDIO
#include "stb_image_write.h"

// ---- vsock plumbing -----------------------------------------------------

static int connect_host(unsigned port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_vm a;
    memset(&a, 0, sizeof(a));
    a.svm_family = AF_VSOCK;
    a.svm_cid = VMADDR_CID_HOST;
    a.svm_port = port;
    if (connect(fd, (struct sockaddr *)&a, sizeof(a)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

// Read exactly n bytes; returns 0 on success, -1 on EOF/error.
static int read_exact(int fd, void *buf, size_t n) {
    char *p = (char *)buf;
    while (n > 0) {
        ssize_t r = read(fd, p, n);
        if (r < 0) { if (errno == EINTR) continue; return -1; }
        if (r == 0) return -1;
        p += r; n -= (size_t)r;
    }
    return 0;
}

static int write_all(int fd, const void *buf, size_t n) {
    const char *p = (const char *)buf;
    while (n > 0) {
        ssize_t w = write(fd, p, n);
        if (w < 0) { if (errno == EINTR) continue; return -1; }
        p += w; n -= (size_t)w;
    }
    return 0;
}

// Send one framed response: [u32 LE header_len][header][payload].
static int send_frame(int fd, const char *header, size_t hlen,
                      const unsigned char *payload, size_t plen) {
    uint32_t le = (uint32_t)hlen; // host is little-endian (arm64/x86_64)
    unsigned char lenbuf[4];
    lenbuf[0] = (unsigned char)(le & 0xff);
    lenbuf[1] = (unsigned char)((le >> 8) & 0xff);
    lenbuf[2] = (unsigned char)((le >> 16) & 0xff);
    lenbuf[3] = (unsigned char)((le >> 24) & 0xff);
    if (write_all(fd, lenbuf, 4) < 0) return -1;
    if (write_all(fd, header, hlen) < 0) return -1;
    if (plen > 0 && write_all(fd, payload, plen) < 0) return -1;
    return 0;
}

static int send_ok(int fd) {
    static const char h[] = "{\"ok\":true,\"payload_len\":0}";
    return send_frame(fd, h, sizeof(h) - 1, NULL, 0);
}

static int send_err(int fd, const char *msg) {
    char h[512];
    // msg is internal/controlled; escape quotes/backslashes defensively.
    char esc[400];
    size_t j = 0;
    for (size_t i = 0; msg[i] && j < sizeof(esc) - 2; i++) {
        if (msg[i] == '"' || msg[i] == '\\') esc[j++] = '\\';
        esc[j++] = msg[i];
    }
    esc[j] = '\0';
    int n = snprintf(h, sizeof(h),
                     "{\"ok\":false,\"error\":\"%s\",\"payload_len\":0}", esc);
    if (n < 0) return -1;
    return send_frame(fd, h, (size_t)n, NULL, 0);
}

static int send_coords(int fd, int x, int y) {
    char h[128];
    int n = snprintf(h, sizeof(h),
                     "{\"ok\":true,\"x\":%d,\"y\":%d,\"payload_len\":0}", x, y);
    if (n < 0) return -1;
    return send_frame(fd, h, (size_t)n, NULL, 0);
}

// ---- minimal JSON extraction (host-controlled schema) -------------------
// The request header is small JSON produced by serde with a known shape:
//   {"action":"<name>", <named fields...>}
// We extract the fields we need by key. Not a general JSON parser, but the
// producer is trusted (our own Rust desktop module) and the schema is fixed.

// Find the value for "key" and copy a JSON string value into out.
// Returns 1 on success, 0 if key absent.
static int json_str(const char *j, const char *key, char *out, size_t outsz) {
    char pat[64];
    snprintf(pat, sizeof(pat), "\"%s\"", key);
    const char *p = strstr(j, pat);
    if (!p) return 0;
    p += strlen(pat);
    while (*p == ' ' || *p == ':') p++;
    if (*p != '"') return 0;
    p++;
    size_t i = 0;
    while (*p && *p != '"' && i < outsz - 1) {
        if (*p == '\\' && p[1]) {
            p++;
            switch (*p) {
                case 'n': out[i++] = '\n'; break;
                case 't': out[i++] = '\t'; break;
                case 'r': out[i++] = '\r'; break;
                default:  out[i++] = *p;   break;
            }
            p++;
        } else {
            out[i++] = *p++;
        }
    }
    out[i] = '\0';
    return 1;
}

// Extract an integer value for "key". Returns 1 on success.
static int json_int(const char *j, const char *key, long *out) {
    char pat[64];
    snprintf(pat, sizeof(pat), "\"%s\"", key);
    const char *p = strstr(j, pat);
    if (!p) return 0;
    p += strlen(pat);
    while (*p == ' ' || *p == ':') p++;
    char *end = NULL;
    long v = strtol(p, &end, 10);
    if (end == p) return 0;
    *out = v;
    return 1;
}

// ---- X helpers ----------------------------------------------------------

static Display *g_dpy;
static Window g_root;
static int g_screen;

static void move_pointer(int x, int y) {
    XTestFakeMotionEvent(g_dpy, g_screen, x, y, CurrentTime);
    XFlush(g_dpy);
}

static void click_button(unsigned button) {
    XTestFakeButtonEvent(g_dpy, button, True, CurrentTime);
    XTestFakeButtonEvent(g_dpy, button, False, CurrentTime);
    XFlush(g_dpy);
}

// Map a key-name token to a keysym, accepting common aliases.
static KeySym name_to_keysym(const char *name) {
    if (!strcasecmp(name, "ctrl") || !strcasecmp(name, "control"))
        return XK_Control_L;
    if (!strcasecmp(name, "alt") || !strcasecmp(name, "option"))
        return XK_Alt_L;
    if (!strcasecmp(name, "shift")) return XK_Shift_L;
    if (!strcasecmp(name, "super") || !strcasecmp(name, "cmd") ||
        !strcasecmp(name, "meta") || !strcasecmp(name, "win"))
        return XK_Super_L;
    if (!strcasecmp(name, "enter")) return XK_Return;
    if (!strcasecmp(name, "esc")) return XK_Escape;
    if (!strcasecmp(name, "del")) return XK_Delete;
    if (!strcasecmp(name, "space")) return XK_space;
    if (!strcasecmp(name, "tab")) return XK_Tab;
    if (!strcasecmp(name, "pgup")) return XK_Prior;
    if (!strcasecmp(name, "pgdn")) return XK_Next;
    // Fall back to the X keysym database ("Return", "ctrl"→no, "a", "F5"...).
    KeySym ks = XStringToKeysym(name);
    return ks;
}

// Press a chord like "ctrl+c", "alt+Tab", "Return". Presses tokens in
// order, then releases in reverse.
static int do_key_chord(const char *spec) {
    char buf[256];
    strncpy(buf, spec, sizeof(buf) - 1);
    buf[sizeof(buf) - 1] = '\0';

    KeyCode codes[16];
    int n = 0;
    char *save = NULL;
    for (char *tok = strtok_r(buf, "+", &save);
         tok && n < 16;
         tok = strtok_r(NULL, "+", &save)) {
        KeySym ks = name_to_keysym(tok);
        if (ks == NoSymbol) return -1;
        KeyCode kc = XKeysymToKeycode(g_dpy, ks);
        if (kc == 0) return -1;
        codes[n++] = kc;
    }
    if (n == 0) return -1;
    for (int i = 0; i < n; i++)
        XTestFakeKeyEvent(g_dpy, codes[i], True, CurrentTime);
    for (int i = n - 1; i >= 0; i--)
        XTestFakeKeyEvent(g_dpy, codes[i], False, CurrentTime);
    XFlush(g_dpy);
    return 0;
}

// Decode one UTF-8 codepoint from p; advance *p. Returns codepoint or -1.
static long utf8_next(const char **p) {
    const unsigned char *s = (const unsigned char *)*p;
    if (s[0] == 0) return -1;
    long cp;
    int extra;
    if (s[0] < 0x80) { cp = s[0]; extra = 0; }
    else if ((s[0] & 0xe0) == 0xc0) { cp = s[0] & 0x1f; extra = 1; }
    else if ((s[0] & 0xf0) == 0xe0) { cp = s[0] & 0x0f; extra = 2; }
    else if ((s[0] & 0xf8) == 0xf0) { cp = s[0] & 0x07; extra = 3; }
    else { cp = 0xfffd; extra = 0; }
    for (int i = 1; i <= extra; i++) {
        if ((s[i] & 0xc0) != 0x80) { cp = 0xfffd; extra = i - 1; break; }
        cp = (cp << 6) | (s[i] & 0x3f);
    }
    *p = (const char *)(s + 1 + extra);
    return cp;
}

// Type a UTF-8 string by remapping a spare keycode to each character's
// keysym (the robust xdotool approach: works for arbitrary characters
// regardless of the current layout).
static int do_type(const char *text) {
    int min_kc, max_kc;
    XDisplayKeycodes(g_dpy, &min_kc, &max_kc);
    // Pick a spare keycode near the top of the range to remap.
    KeyCode spare = (KeyCode)max_kc;

    // The server stores several keysyms per keycode (shift levels etc.). We
    // must remap with that exact width — passing 1 leaves the keycode's real
    // mapping untouched, so every char would emit the spare's stale keysym.
    int per = 1;
    KeySym *probe = XGetKeyboardMapping(g_dpy, spare, 1, &per);
    if (probe) XFree(probe);
    if (per < 1) per = 1;

    const char *p = text;
    long cp;
    while ((cp = utf8_next(&p)) >= 0) {
        KeySym ks = (cp < 0x100) ? (KeySym)cp : (KeySym)(0x01000000 | cp);
        KeySym map[8];
        int levels = per > 8 ? 8 : per;
        for (int i = 0; i < levels; i++) map[i] = ks; // unshifted + shifted
        XChangeKeyboardMapping(g_dpy, spare, levels, map, 1);
        XSync(g_dpy, False);
        // Let the focused client process the MappingNotify before the key.
        usleep(2000);
        XTestFakeKeyEvent(g_dpy, spare, True, CurrentTime);
        XTestFakeKeyEvent(g_dpy, spare, False, CurrentTime);
        XSync(g_dpy, False);
        usleep(2000);
    }
    return 0;
}

// ---- screenshot (XGetImage → PNG) --------------------------------------

struct png_buf {
    unsigned char *data;
    size_t len, cap;
    int oom;
};

static void png_sink(void *ctx, void *data, int size) {
    struct png_buf *b = (struct png_buf *)ctx;
    if (b->oom) return;
    if (b->len + (size_t)size > b->cap) {
        size_t ncap = (b->cap ? b->cap * 2 : 1 << 16);
        while (ncap < b->len + (size_t)size) ncap *= 2;
        unsigned char *nd = (unsigned char *)realloc(b->data, ncap);
        if (!nd) { b->oom = 1; return; }
        b->data = nd; b->cap = ncap;
    }
    memcpy(b->data + b->len, data, (size_t)size);
    b->len += (size_t)size;
}

static int mask_shift(unsigned long mask) {
    int s = 0;
    if (!mask) return 0;
    while (!(mask & 1)) { mask >>= 1; s++; }
    return s;
}

static int do_screenshot(int fd) {
    XWindowAttributes wa;
    XGetWindowAttributes(g_dpy, g_root, &wa);
    int w = wa.width, h = wa.height;

    XImage *img = XGetImage(g_dpy, g_root, 0, 0, w, h, AllPlanes, ZPixmap);
    if (!img) return send_err(fd, "XGetImage failed");

    unsigned char *rgb = (unsigned char *)malloc((size_t)w * h * 3);
    if (!rgb) { XDestroyImage(img); return send_err(fd, "oom"); }

    int rs = mask_shift(img->red_mask);
    int gs = mask_shift(img->green_mask);
    int bs = mask_shift(img->blue_mask);

    for (int y = 0; y < h; y++) {
        for (int x = 0; x < w; x++) {
            unsigned long px = XGetPixel(img, x, y);
            size_t o = ((size_t)y * w + x) * 3;
            rgb[o + 0] = (unsigned char)((px & img->red_mask) >> rs);
            rgb[o + 1] = (unsigned char)((px & img->green_mask) >> gs);
            rgb[o + 2] = (unsigned char)((px & img->blue_mask) >> bs);
        }
    }
    XDestroyImage(img);

    struct png_buf pb = {0};
    int ok = stbi_write_png_to_func(png_sink, &pb, w, h, 3, rgb, w * 3);
    free(rgb);
    if (!ok || pb.oom) { free(pb.data); return send_err(fd, "png encode failed"); }

    char header[128];
    int n = snprintf(header, sizeof(header),
                     "{\"ok\":true,\"payload_len\":%zu}", pb.len);
    int rc = send_frame(fd, header, (size_t)n, pb.data, pb.len);
    free(pb.data);
    return rc;
}

// ---- request dispatch ---------------------------------------------------

static void launch_detached(const char *cmd) {
    pid_t pid = fork();
    if (pid == 0) {
        setsid();
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }
    // Parent does not wait; the app runs in the session.
}

static int handle(int fd, const char *json) {
    char action[64];
    if (!json_str(json, "action", action, sizeof(action)))
        return send_err(fd, "missing action");

    long x = 0, y = 0, amount = 0, ms = 0;

    if (!strcmp(action, "screenshot")) {
        return do_screenshot(fd);
    } else if (!strcmp(action, "cursor_position")) {
        Window r, c; int rx, ry, wx, wy; unsigned int mask;
        if (!XQueryPointer(g_dpy, g_root, &r, &c, &rx, &ry, &wx, &wy, &mask))
            return send_err(fd, "XQueryPointer failed");
        return send_coords(fd, rx, ry);
    } else if (!strcmp(action, "mouse_move")) {
        json_int(json, "x", &x); json_int(json, "y", &y);
        move_pointer((int)x, (int)y);
        return send_ok(fd);
    } else if (!strcmp(action, "left_click")) {
        click_button(1); return send_ok(fd);
    } else if (!strcmp(action, "right_click")) {
        click_button(3); return send_ok(fd);
    } else if (!strcmp(action, "middle_click")) {
        click_button(2); return send_ok(fd);
    } else if (!strcmp(action, "double_click")) {
        click_button(1); usleep(50000); click_button(1);
        return send_ok(fd);
    } else if (!strcmp(action, "left_click_drag")) {
        json_int(json, "x", &x); json_int(json, "y", &y);
        XTestFakeButtonEvent(g_dpy, 1, True, CurrentTime);
        XFlush(g_dpy);
        move_pointer((int)x, (int)y);
        XTestFakeButtonEvent(g_dpy, 1, False, CurrentTime);
        XFlush(g_dpy);
        return send_ok(fd);
    } else if (!strcmp(action, "type")) {
        char text[8192];
        if (!json_str(json, "text", text, sizeof(text)))
            return send_err(fd, "missing text");
        do_type(text);
        return send_ok(fd);
    } else if (!strcmp(action, "key")) {
        char keys[256];
        if (!json_str(json, "keys", keys, sizeof(keys)))
            return send_err(fd, "missing keys");
        if (do_key_chord(keys) < 0) return send_err(fd, "bad key chord");
        return send_ok(fd);
    } else if (!strcmp(action, "scroll")) {
        char dir[16] = "down";
        json_int(json, "x", &x); json_int(json, "y", &y);
        json_int(json, "amount", &amount);
        json_str(json, "direction", dir, sizeof(dir));
        move_pointer((int)x, (int)y);
        unsigned button = 5; // down
        if (!strcmp(dir, "up")) button = 4;
        else if (!strcmp(dir, "down")) button = 5;
        else if (!strcmp(dir, "left")) button = 6;
        else if (!strcmp(dir, "right")) button = 7;
        if (amount <= 0) amount = 1;
        for (long i = 0; i < amount; i++) click_button(button);
        return send_ok(fd);
    } else if (!strcmp(action, "wait")) {
        json_int(json, "ms", &ms);
        if (ms > 0) usleep((useconds_t)(ms * 1000));
        return send_ok(fd);
    } else if (!strcmp(action, "exec")) {
        char cmd[4096];
        if (!json_str(json, "command", cmd, sizeof(cmd)))
            return send_err(fd, "missing command");
        launch_detached(cmd);
        return send_ok(fd);
    }
    return send_err(fd, "unknown action");
}

// ---- main ---------------------------------------------------------------

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s HOST_PORT [DISPLAY]\n", argv[0]);
        return 2;
    }
    unsigned port = (unsigned)strtoul(argv[1], NULL, 10);
    const char *display = (argc >= 3) ? argv[2] : ":99";

    g_dpy = XOpenDisplay(display);
    if (!g_dpy) {
        fprintf(stderr, "agent: cannot open display %s\n", display);
        return 1;
    }
    g_screen = DefaultScreen(g_dpy);
    g_root = RootWindow(g_dpy, g_screen);

    int event_base, error_base, major, minor;
    if (!XTestQueryExtension(g_dpy, &event_base, &error_base, &major, &minor)) {
        fprintf(stderr, "agent: XTEST extension unavailable\n");
        return 1;
    }

    int fd = connect_host(port);
    if (fd < 0) {
        fprintf(stderr, "agent: connect to host:%u failed: %s\n",
                port, strerror(errno));
        return 1;
    }
    fprintf(stderr, "agent: connected to host:%u, serving on %s\n",
            port, display);

    for (;;) {
        unsigned char lenbuf[4];
        if (read_exact(fd, lenbuf, 4) < 0) break; // host closed
        uint32_t hlen = (uint32_t)lenbuf[0] | ((uint32_t)lenbuf[1] << 8) |
                        ((uint32_t)lenbuf[2] << 16) | ((uint32_t)lenbuf[3] << 24);
        if (hlen == 0 || hlen > (1u << 20)) break;
        char *hdr = (char *)malloc(hlen + 1);
        if (!hdr) break;
        if (read_exact(fd, hdr, hlen) < 0) { free(hdr); break; }
        hdr[hlen] = '\0';
        int rc = handle(fd, hdr);
        free(hdr);
        if (rc < 0) break; // write failed → host gone
    }

    close(fd);
    XCloseDisplay(g_dpy);
    return 0;
}
