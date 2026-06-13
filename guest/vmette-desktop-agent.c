// vmette-desktop-agent — in-guest computer-use agent for vmette's desktop
// (Agent) workload. Drives a headless Xvfb display via XTEST (synthetic
// input) and XGetImage (framebuffer capture), speaking vmette's framed
// protocol over AF_VSOCK to the host vmette::Session.
//
// Unlike the static musl initramfs helpers (vsock-send / vsock-runner),
// this binary links libX11 + libXtst dynamically and therefore lives
// *inside* the desktop rootfs image, compiled against that image's libc.
//
// Synthetic typing (do_type) distills xdotool's scratch-keycode technique:
// arbitrary text needs keysyms no fixed layout carries on a real key, so we
// temporarily bind spare keycodes to the characters' keysyms and fake the keys.
// Unlike xdotool we bind every distinct character of a string up front and sync
// once, then fire all keystrokes against that fixed mapping — so no keymap
// change ever races a synthetic key. See do_type for the full rationale.
//
// Wire protocol (must match crates/vmette/src/desktop.rs):
//   request  (host → guest): [u32 LE req_id][u32 LE header_len][header JSON]            (no payload)
//   response (guest → host): [u32 LE req_id][u32 LE header_len][header JSON][payload]   (payload optional)
// The host tags each request with a monotonic req_id and demultiplexes
// responses by it; the agent echoes the same req_id back. The response header
// carries "payload_len" = number of trailing bytes (a PNG for screenshots; 0
// otherwise).
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
#include <sys/select.h>
#include <sys/wait.h>
#include <sys/time.h>
#include <linux/vm_sockets.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/Xatom.h>
#include <X11/keysym.h>
#include <X11/extensions/XTest.h>
#include <X11/extensions/Xfixes.h>

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

// Write a little-endian u32 into a 4-byte buffer.
static void put_u32_le(unsigned char *b, uint32_t v) {
    b[0] = (unsigned char)(v & 0xff);
    b[1] = (unsigned char)((v >> 8) & 0xff);
    b[2] = (unsigned char)((v >> 16) & 0xff);
    b[3] = (unsigned char)((v >> 24) & 0xff);
}

// Send one framed response: [u32 LE req_id][u32 LE header_len][header][payload].
// `req_id` echoes the request's id so the host can route this reply to its
// caller. It is an EXPLICIT parameter (not a global) because replies can be
// deferred — an async exec_capture replies long after the next request has been
// read, so each reply must carry the id captured when its request arrived.
static int send_frame(int fd, uint32_t req_id, const char *header, size_t hlen,
                      const unsigned char *payload, size_t plen) {
    unsigned char prefix[8]; // host is little-endian (arm64/x86_64)
    put_u32_le(prefix, req_id);
    put_u32_le(prefix + 4, (uint32_t)hlen);
    if (write_all(fd, prefix, 8) < 0) return -1;
    if (write_all(fd, header, hlen) < 0) return -1;
    if (plen > 0 && write_all(fd, payload, plen) < 0) return -1;
    return 0;
}

static int send_ok(int fd, uint32_t req_id) {
    static const char h[] = "{\"ok\":true,\"payload_len\":0}";
    return send_frame(fd, req_id, h, sizeof(h) - 1, NULL, 0);
}

static int send_err(int fd, uint32_t req_id, const char *msg) {
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
    return send_frame(fd, req_id, h, (size_t)n, NULL, 0);
}

static int send_coords(int fd, uint32_t req_id, int x, int y) {
    char h[128];
    int n = snprintf(h, sizeof(h),
                     "{\"ok\":true,\"x\":%d,\"y\":%d,\"payload_len\":0}", x, y);
    if (n < 0) return -1;
    return send_frame(fd, req_id, h, (size_t)n, NULL, 0);
}

// Success reply whose `plen` payload bytes (a PNG, clipboard text, …) follow
// the header. `plen == 0` is the bare-ok case.
static int send_payload(int fd, uint32_t req_id, const unsigned char *payload,
                        size_t plen) {
    char h[64];
    int n = snprintf(h, sizeof(h), "{\"ok\":true,\"payload_len\":%zu}", plen);
    if (n < 0) return -1;
    return send_frame(fd, req_id, h, (size_t)n, payload, plen);
}

// Reply for an exec_capture: `plen` payload bytes (the combined stdout/stderr)
// follow the header, and the header carries the child's exit code. A negative
// `exit_code` means the child was killed (e.g. timeout) and is serialized as
// JSON `null`, which the host reads as "no clean exit".
static int send_exec_result(int fd, uint32_t req_id, int exit_code,
                            const unsigned char *payload, size_t plen) {
    char h[96];
    char code[16];
    if (exit_code < 0)
        snprintf(code, sizeof(code), "null");
    else
        snprintf(code, sizeof(code), "%d", exit_code);
    int n = snprintf(h, sizeof(h),
                     "{\"ok\":true,\"exit_code\":%s,\"payload_len\":%zu}", code,
                     plen);
    if (n < 0) return -1;
    return send_frame(fd, req_id, h, (size_t)n, payload, plen);
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

// ---- clipboard (X selection) state --------------------------------------
// An unmapped window owns the CLIPBOARD/PRIMARY selections so the agent can
// serve paste requests; g_clip holds the bytes we serve (set_clipboard), freed
// when another client takes ownership (SelectionClear).
static Window g_clip_win;
static Atom A_CLIPBOARD, A_TARGETS, A_UTF8, A_TEXT, A_INCR, A_PROP;
static char *g_clip;
static size_t g_clip_len;

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

// Latin-1 codepoints map 1:1 to their keysym; everything else uses the X11
// Unicode keysym range (0x01000000 | codepoint).
static KeySym cp_to_keysym(long cp) {
    // Control characters callers expect to act as keys, not literal glyphs:
    // newline / carriage-return submit (Return) and tab indents (Tab). Without
    // this a typed "\n" maps to keysym 0x0A, which clients do not treat as
    // Enter, so multi-line desktop_type input never advances a line.
    if (cp == 0x0A || cp == 0x0D) return XK_Return;
    if (cp == 0x09) return XK_Tab;
    return (cp < 0x100) ? (KeySym)cp : (KeySym)(0x01000000 | cp);
}

// Type a UTF-8 string. This distills xdotool's scratch-keycode technique
// (xdo_send_keysequence_window_list_do) but hoists the binding out of the
// per-keystroke hot path: xdotool rebinds and reverts a single scratch keycode
// around *every* keystroke, so each synthetic KeyPress races the focused
// client's keymap-cache refresh (the client must process the MappingNotify and
// call XRefreshKeyboardMapping before it can decode the key). Under a
// software-rendered Xvfb in a VM that race loses intermittently — text comes
// out with dropped, duplicated, or stale characters.
//
// Instead we bind each *distinct* character of the string to its own scratch
// keycode up front, sync once, and let the client refresh its keymap a single
// time. We then fire every keystroke against that now-fixed mapping with no
// further mapping changes, so no MappingNotify ever interleaves with a key
// event — the race is gone by construction. The mapping is re-established on
// every call, so Xvfb's XKB layer (which silently recompiles its keymap and
// reverts core-protocol remaps between calls) cannot strand us mid-session, and
// repeated characters cost no extra remap.
//
// When a string has more *distinct* codepoints than we have scratch keycodes,
// we type it in SEGMENTS: bind a segment's distinct chars up front, sync once,
// type that segment, revert, then rebind for the next segment. Every keystroke
// still fires against a fixed mapping (the race stays gone), and no keycode is
// ever reused mid-string — unlike the earlier single-pass scheme, whose
// per-keystroke "overflow" slot reused freekc[nfree-1] (also the last bound
// keycode), clobbering it and corrupting any string with >= nfree distinct
// characters.
//
// We map BOTH shift levels to the same keysym ({ks, ks}) so the bare keycode
// emits the exact character with no modifier: X case-folds a lone alphabetic
// keysym to lowercase at level 0, and binding level 1 explicitly sidesteps that
// (xdotool instead holds Shift for uppercase).
static int do_type(const char *text) {
    int lo, hi, per = 0;
    XDisplayKeycodes(g_dpy, &lo, &hi);
    KeySym *km = XGetKeyboardMapping(g_dpy, lo, hi - lo + 1, &per);
    if (!km || per < 1) {
        if (km) XFree(km);
        return -1;
    }

    // Scratch keycodes are those whose level-0 keysym is NoSymbol — unused, so
    // safe to rebind without clobbering a real key.
    KeyCode freekc[256];
    int nfree = 0;
    for (int kc = lo;
         kc <= hi && nfree < (int)(sizeof(freekc) / sizeof(freekc[0])); kc++) {
        if (km[(kc - lo) * per] == NoSymbol) freekc[nfree++] = (KeyCode)kc;
    }
    XFree(km);
    if (nfree == 0) return -1;

    const char *p = text;
    int max_bound = 0; // high-water mark of scratch keycodes used, for cleanup
    while (*p) {
        // Pass 1: extend a segment from `p`, binding each new distinct codepoint
        // to its own scratch keycode, until the next new one would exceed the
        // keycode pool. `seg_end` marks the (exclusive) end of the segment.
        long bound_cp[256];
        int nbound = 0;
        const char *seg_end = p;
        for (const char *q = p; *q;) {
            long cp = utf8_next(&q);
            int seen = 0;
            for (int b = 0; b < nbound; b++)
                if (bound_cp[b] == cp) { seen = 1; break; }
            if (!seen) {
                if (nbound == nfree) break; // pool full: end the segment here
                KeySym ks = cp_to_keysym(cp);
                KeySym list[2] = { ks, ks };
                XChangeKeyboardMapping(g_dpy, freekc[nbound], 2, list, 1);
                bound_cp[nbound++] = cp;
            }
            seg_end = q; // this char is part of the segment
        }
        if (nbound > max_bound) max_bound = nbound;

        // Sync + settle so the focused client processes the MappingNotify and
        // refreshes its keymap before we fire keystrokes against it. A new
        // segment rebinds keycodes the previous one used, so its *first* strokes
        // race that refresh; this settle (longer than a single bind needs)
        // covers the boundary. Keycodes are NOT reverted between segments — a
        // stroke that still beats the refresh then decodes as the prior glyph
        // (recoverable) rather than NoSymbol (a silent drop).
        XSync(g_dpy, False);
        usleep(40000);

        // Pass 2: fire the segment's characters against the now-stable mapping.
        for (const char *q = p; q < seg_end;) {
            long cp = utf8_next(&q);
            KeyCode kc = 0;
            for (int b = 0; b < nbound; b++)
                if (bound_cp[b] == cp) { kc = freekc[b]; break; }
            if (kc == 0) continue; // defensive: every segment char was bound
            XTestFakeKeyEvent(g_dpy, kc, True, CurrentTime);
            usleep(6000); // split inter-key delay across down/up, like xdotool
            XTestFakeKeyEvent(g_dpy, kc, False, CurrentTime);
            XSync(g_dpy, False);
            usleep(6000);
        }

        p = seg_end; // always advances: the first char of a segment always binds
    }

    // Revert every scratch keycode we touched back to NoSymbol, once.
    KeySym none[2] = { NoSymbol, NoSymbol };
    for (int b = 0; b < max_bound; b++)
        XChangeKeyboardMapping(g_dpy, freekc[b], 2, none, 1);
    XSync(g_dpy, False);
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

// Composite the X pointer sprite onto the captured RGB buffer. XGetImage does
// NOT include the cursor — the X server draws it as a separate overlay — so a
// raw screenshot has no pointer, leaving it invisible in the live view and in
// the agent's own screenshots. XFixes hands us the sprite (premultiplied-alpha
// ARGB) and its on-screen position; we alpha-blend it over the frame. Best
// effort: if XFixes is unavailable or no cursor is set, the frame is unchanged.
static void composite_cursor(unsigned char *rgb, int w, int h) {
    XFixesCursorImage *ci = XFixesGetCursorImage(g_dpy);
    if (!ci) return;
    int ox = (int)ci->x - (int)ci->xhot;  // sprite top-left on screen
    int oy = (int)ci->y - (int)ci->yhot;
    for (int cy = 0; cy < (int)ci->height; cy++) {
        int sy = oy + cy;
        if (sy < 0 || sy >= h) continue;
        for (int cx = 0; cx < (int)ci->width; cx++) {
            int sx = ox + cx;
            if (sx < 0 || sx >= w) continue;
            // Each pixel is 32-bit premultiplied-alpha ARGB in the low bits of
            // an `unsigned long`.
            unsigned long p = ci->pixels[(size_t)cy * ci->width + cx];
            unsigned a = (p >> 24) & 0xff;
            if (!a) continue;  // fully transparent — leave the frame pixel
            unsigned cr = (p >> 16) & 0xff;  // colours already premultiplied
            unsigned cg = (p >> 8) & 0xff;
            unsigned cb = p & 0xff;
            size_t o = ((size_t)sy * w + sx) * 3;
            unsigned ia = 255 - a;  // "over": out = src + dst*(1-a)
            unsigned r = cr + rgb[o + 0] * ia / 255;
            unsigned g = cg + rgb[o + 1] * ia / 255;
            unsigned b = cb + rgb[o + 2] * ia / 255;
            rgb[o + 0] = r > 255 ? 255 : (unsigned char)r;
            rgb[o + 1] = g > 255 ? 255 : (unsigned char)g;
            rgb[o + 2] = b > 255 ? 255 : (unsigned char)b;
        }
    }
    XFree(ci);
}

static int do_screenshot(int fd, uint32_t req_id) {
    XWindowAttributes wa;
    if (!XGetWindowAttributes(g_dpy, g_root, &wa))
        return send_err(fd, req_id,"XGetWindowAttributes failed");
    int w = wa.width, h = wa.height;

    XImage *img = XGetImage(g_dpy, g_root, 0, 0, w, h, AllPlanes, ZPixmap);
    if (!img) return send_err(fd, req_id,"XGetImage failed");

    unsigned char *rgb = (unsigned char *)malloc((size_t)w * h * 3);
    if (!rgb) { XDestroyImage(img); return send_err(fd, req_id,"oom"); }

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

    // Draw the pointer in (XGetImage omits it), so the cursor is visible in the
    // live view and in computer-use screenshots.
    composite_cursor(rgb, w, h);

    struct png_buf pb = {0};
    int ok = stbi_write_png_to_func(png_sink, &pb, w, h, 3, rgb, w * 3);
    free(rgb);
    if (!ok || pb.oom) { free(pb.data); return send_err(fd, req_id,"png encode failed"); }

    int rc = send_payload(fd, req_id,pb.data, pb.len);
    free(pb.data);
    return rc;
}

// ---- clipboard (X selections) -------------------------------------------
//
// X has no clipboard store: the *owner* of a selection must stay alive and
// answer SelectionRequest events when an app pastes. The agent is that owner —
// g_clip_win holds CLIPBOARD + PRIMARY and serve_selection() answers requests
// from the main loop's X-event pump. Large transfers (INCR) are not served;
// clipboard text far beyond a single property request is uncommon.

// Take ownership of `text` (a `len`-byte, NUL-terminated, heap buffer) as the
// clipboard and own both selections, so the bytes paste in GUI apps (Ctrl+V,
// CLIPBOARD) and terminals (Shift+Insert / middle-click, PRIMARY) alike. The
// agent owns `text` hereafter (freed on the next set or on SelectionClear).
static void do_set_clipboard(char *text, size_t len) {
    free(g_clip);
    g_clip = text;
    g_clip_len = len;
    XSetSelectionOwner(g_dpy, A_CLIPBOARD, g_clip_win, CurrentTime);
    XSetSelectionOwner(g_dpy, XA_PRIMARY, g_clip_win, CurrentTime);
    XFlush(g_dpy);
}

// Answer one selection event: serve TARGETS / UTF8_STRING / STRING / TEXT for a
// SelectionRequest, or drop our copy on SelectionClear (we lost ownership).
static void serve_selection(XEvent *ev) {
    if (ev->type == SelectionClear) {
        free(g_clip);
        g_clip = NULL;
        g_clip_len = 0;
        return;
    }
    if (ev->type != SelectionRequest) return;

    XSelectionRequestEvent *req = &ev->xselectionrequest;
    XSelectionEvent resp;
    memset(&resp, 0, sizeof(resp));
    resp.type = SelectionNotify;
    resp.display = req->display;
    resp.requestor = req->requestor;
    resp.selection = req->selection;
    resp.target = req->target;
    resp.time = req->time;
    // Obsolete clients send property=None meaning "use the target atom".
    resp.property = req->property ? req->property : req->target;

    if (!g_clip) {
        resp.property = None;
    } else if (req->target == A_TARGETS) {
        Atom targets[] = {A_TARGETS, A_UTF8, XA_STRING, A_TEXT};
        XChangeProperty(g_dpy, req->requestor, resp.property, XA_ATOM, 32,
                        PropModeReplace, (unsigned char *)targets,
                        (int)(sizeof(targets) / sizeof(targets[0])));
    } else if (req->target == A_UTF8 || req->target == XA_STRING ||
               req->target == A_TEXT) {
        XChangeProperty(g_dpy, req->requestor, resp.property, req->target, 8,
                        PropModeReplace, (unsigned char *)g_clip,
                        (int)g_clip_len);
    } else {
        resp.property = None; // unsupported target
    }
    XSendEvent(g_dpy, req->requestor, False, 0, (XEvent *)&resp);
    XFlush(g_dpy);
}

// Read the CLIPBOARD selection and reply with its text as the frame payload.
static int do_get_clipboard(int fd, uint32_t req_id) {
    // Fast path: we own it, so answer from our own copy without a round-trip.
    if (g_clip && XGetSelectionOwner(g_dpy, A_CLIPBOARD) == g_clip_win) {
        return send_payload(fd, req_id,(unsigned char *)g_clip, g_clip_len);
    }
    int xfd = ConnectionNumber(g_dpy);
    // (Re)issue the conversion until an owner answers with data, or until a
    // deadline. Re-issuing is what makes the *first* read after a Ctrl+C
    // reliable: a GUI app (e.g. the browser) asserts CLIPBOARD ownership
    // asynchronously, so a convert fired immediately after the copy keystroke
    // can race ahead of that and get a "no owner" reply (SelectionNotify with
    // property == None). Rather than returning a spuriously-empty clipboard, we
    // retry across that ownership handoff. A genuinely unset clipboard simply
    // costs this bounded wait before returning empty. Bounded so a missing or
    // buggy owner can't hang the agent.
    struct timeval start, now;
    gettimeofday(&start, NULL);
    int need_convert = 1;
    for (;;) {
        if (need_convert) {
            XDeleteProperty(g_dpy, g_clip_win, A_PROP);
            XConvertSelection(g_dpy, A_CLIPBOARD, A_UTF8, A_PROP, g_clip_win,
                              CurrentTime);
            XFlush(g_dpy);
            need_convert = 0;
        }
        while (XPending(g_dpy)) {
            XEvent ev;
            XNextEvent(g_dpy, &ev);
            if (ev.type == SelectionNotify &&
                ev.xselection.requestor == g_clip_win &&
                ev.xselection.selection == A_CLIPBOARD) {
                if (ev.xselection.property == None) {
                    // No owner / nothing to convert yet — retry until deadline.
                    need_convert = 1;
                    break;
                }
                Atom type;
                int format;
                unsigned long nitems, after;
                unsigned char *data = NULL;
                XGetWindowProperty(g_dpy, g_clip_win, A_PROP, 0, ~0L, True,
                                   AnyPropertyType, &type, &format, &nitems,
                                   &after, &data);
                if (type == A_INCR) {
                    if (data) XFree(data);
                    return send_err(fd, req_id,"clipboard too large (INCR unsupported)");
                }
                size_t len = (data && format == 8) ? (size_t)nitems : 0;
                int rc = send_payload(fd, req_id,data, len);
                if (data) XFree(data);
                return rc;
            }
            serve_selection(&ev);
        }
        gettimeofday(&now, NULL);
        long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000L +
                          (now.tv_usec - start.tv_usec) / 1000L;
        if (elapsed_ms >= 1500) {
            // Bounded out: an unset/unavailable clipboard is a normal state, so
            // report it as empty rather than an error.
            return send_payload(fd, req_id,NULL, 0);
        }
        if (need_convert) {
            // Got a "no owner" answer; pause briefly so we don't spin re-issuing
            // the convert before the copying app can take ownership.
            usleep(50000); // 50ms
        } else {
            // Awaiting the notify from a present-but-slow owner; block on the X
            // connection (no busy spin) until data arrives or 100ms passes.
            fd_set r;
            FD_ZERO(&r);
            FD_SET(xfd, &r);
            struct timeval tv = {.tv_sec = 0, .tv_usec = 100000};
            select(xfd + 1, &r, NULL, NULL, &tv);
        }
    }
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

// Spawn `prog` with the single argument `arg`, detached, **without a shell**.
// Because `arg` is passed as one execlp argv element it is never word-split,
// glob-expanded, or interpreted — so a hostile URL/path can't inject commands.
// Used by the `navigate` action to hand a URL to the `vmette-open` launcher.
static void launch_argv(const char *prog, const char *arg) {
    pid_t pid = fork();
    if (pid == 0) {
        setsid();
        execlp(prog, prog, arg, (char *)NULL);
        _exit(127);
    }
    // Parent does not wait; the app runs in the session.
}

// ---- async jobs: exec_capture + wait ------------------------------------
//
// `exec_capture` (run a command to completion) and `wait` (sleep) are the only
// actions that block for a meaningful time. Running them synchronously froze
// the whole agent — input, screenshots, and X selection serving all stalled
// until they returned. Instead each is registered as an async JOB and the main
// select() loop drives it to completion while still serving other requests,
// replying out of order with the job's captured `req_id` (the host
// demultiplexes by `req_id`, so out-of-order replies are fine).
//
// The agent stays strictly single-threaded: a job is just an fd + a deadline in
// the one loop, so nothing touches the X display off-thread and input stays
// ordered. Only these two non-X actions defer.
#define EXEC_CAPTURE_MAX_OUTPUT (256 * 1024)
#define MAX_JOBS 16
// After an exec job's pipe hits EOF the child has usually exited — but the
// kernel closes the pipe write end a hair before the task becomes reapable, so
// `waitpid(WNOHANG)` often returns 0 right at EOF for a perfectly clean exit. We
// therefore *poll* for the exit (every EXEC_EOF_POLL_MS) instead of assuming the
// child is detached, and only force-kill it as genuinely-detached (e.g. `exec
// foo >/dev/null`) once EXEC_EOF_GRACE_MS has passed with it still alive. The
// grace is generous precisely because the poll is non-blocking — it costs no
// interactivity, so a clean-but-slow-to-reap exit is never mis-killed.
#define EXEC_EOF_GRACE_MS 2000
#define EXEC_EOF_POLL_MS 5

struct job {
    int active;
    int is_wait;             // wait timer (no child/pipe) vs exec_capture
    int eof;                 // exec: pipe drained, now awaiting the child's exit
    uint32_t req_id;         // captured at submit; echoed in the deferred reply
    pid_t pid;               // exec only
    int outfd;               // exec only: read end of the child's stdout pipe (-1 once EOF'd)
    unsigned char *buf;      // exec only: captured output (capped), malloc'd
    size_t len;
    struct timeval deadline; // exec: run-timeout SIGKILL at (or, once eof, the
                             // detached force-kill grace); wait: reply ok at
};
static struct job g_jobs[MAX_JOBS];

static struct job *job_alloc(void) {
    for (int i = 0; i < MAX_JOBS; i++)
        if (!g_jobs[i].active) return &g_jobs[i];
    return NULL;
}

// `ms` from now as an absolute deadline.
static struct timeval deadline_in(long ms) {
    struct timeval t;
    gettimeofday(&t, NULL);
    t.tv_sec += ms / 1000;
    t.tv_usec += (ms % 1000) * 1000;
    if (t.tv_usec >= 1000000) { t.tv_sec += 1; t.tv_usec -= 1000000; }
    return t;
}

// Milliseconds from now until `t` (negative once it has passed).
static long ms_until(const struct timeval *t) {
    struct timeval now;
    gettimeofday(&now, NULL);
    return (t->tv_sec - now.tv_sec) * 1000L + (t->tv_usec - now.tv_usec) / 1000L;
}

// Register an async exec_capture: fork the child with stdout+stderr on a pipe,
// stash the job, and return WITHOUT replying — the reply is sent later by
// drain_job (child EOF) or expire_jobs (timeout). Setup failures reply at once.
static void start_exec_capture(int fd, uint32_t req_id, const char *cmd,
                               long timeout_ms) {
    struct job *j = job_alloc();
    if (!j) { send_err(fd, req_id, "too many concurrent jobs"); return; }
    unsigned char *buf = (unsigned char *)malloc(EXEC_CAPTURE_MAX_OUTPUT);
    if (!buf) { send_err(fd, req_id, "oom"); return; }
    int pipefd[2];
    if (pipe(pipefd) != 0) {
        free(buf);
        send_err(fd, req_id, "pipe failed");
        return;
    }
    pid_t pid = fork();
    if (pid < 0) {
        close(pipefd[0]); close(pipefd[1]); free(buf);
        send_err(fd, req_id, "fork failed");
        return;
    }
    if (pid == 0) {
        // Child: route stdout+stderr into the pipe, detach the controlling tty,
        // then exec the shell. stdin is /dev/null so a command that reads it
        // sees EOF rather than blocking forever.
        setsid();
        dup2(pipefd[1], STDOUT_FILENO);
        dup2(pipefd[1], STDERR_FILENO);
        close(pipefd[0]);
        close(pipefd[1]);
        int devnull = open("/dev/null", O_RDONLY);
        if (devnull >= 0) {
            dup2(devnull, STDIN_FILENO);
            if (devnull > STDIN_FILENO) close(devnull);
        }
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }
    close(pipefd[1]);
    j->active = 1; j->is_wait = 0; j->eof = 0; j->req_id = req_id; j->pid = pid;
    j->outfd = pipefd[0]; j->buf = buf; j->len = 0;
    j->deadline = deadline_in(timeout_ms);
}

// Register a wait timer: reply ok once the deadline fires (see expire_jobs).
static void start_wait(uint32_t req_id, long ms) {
    struct job *j = job_alloc();
    if (!j) return; // table full: drop (wait is best-effort; host will time out)
    j->active = 1; j->is_wait = 1; j->eof = 0; j->req_id = req_id;
    j->pid = -1; j->outfd = -1; j->buf = NULL; j->len = 0;
    j->deadline = deadline_in(ms);
}

// EINTR-safe blocking reap of a specific child. Bounded: every caller SIGKILLs
// the child first, so the reap returns promptly.
static pid_t reap_pid(pid_t pid, int *wstatus) {
    pid_t w;
    do { w = waitpid(pid, wstatus, 0); } while (w < 0 && errno == EINTR);
    return w;
}

// Non-blocking reap: 1 if the child has been reaped (exited, or already gone),
// 0 if it is still running. `*wstatus` carries the exit status on a real exit;
// it is zeroed for the already-gone (ECHILD) case so the caller reads exit 0.
static int try_reap(pid_t pid, int *wstatus) {
    pid_t w;
    do { w = waitpid(pid, wstatus, WNOHANG); } while (w < 0 && errno == EINTR);
    if (w < 0) *wstatus = 0; // ECHILD: no child to wait on; treat as clean exit
    return w != 0;
}

// Reply exit code: the child's status, or -1 (=> JSON null) when it did not
// exit cleanly (killed/signalled).
static int exit_code_of(int wstatus) {
    return WIFEXITED(wstatus) ? WEXITSTATUS(wstatus) : -1;
}

// Send an exec job's captured result and tear the job down.
static void deliver_exec(int fd, struct job *j, int code) {
    send_exec_result(fd, j->req_id, code, j->buf, j->len);
    if (j->outfd >= 0) close(j->outfd);
    free(j->buf);
    j->active = 0;
    j->eof = 0;
}

// An exec job's pipe is readable: accumulate output (capped). On EOF the output
// is complete; reap the child if it is already done, else mark the job
// `eof`-awaiting so expire_jobs polls for its exit without blocking the loop.
static void drain_job(int fd, struct job *j) {
    if (j->len < EXEC_CAPTURE_MAX_OUTPUT) {
        ssize_t got = read(j->outfd, j->buf + j->len,
                           EXEC_CAPTURE_MAX_OUTPUT - j->len);
        if (got > 0) { j->len += (size_t)got; return; }
        if (got < 0 && errno == EINTR) return;
    } else {
        // Buffer full: keep draining so the child never blocks on a full pipe,
        // but drop the overflow (matches the old cap-and-discard behavior).
        unsigned char scratch[4096];
        ssize_t got = read(j->outfd, scratch, sizeof(scratch));
        if (got > 0) return;
        if (got < 0 && errno == EINTR) return;
    }
    // EOF or hard error.
    int wstatus = 0;
    if (try_reap(j->pid, &wstatus)) {
        deliver_exec(fd, j, exit_code_of(wstatus)); // common case: already exited
    } else {
        // Not reapable yet: either the exit just hasn't landed (the usual race —
        // resolved within a poll or two) or the child detached its stdio and
        // will run on. Stop reading the pipe and await the exit via expire_jobs;
        // the deadline becomes the detached force-kill grace.
        close(j->outfd);
        j->outfd = -1;
        j->eof = 1;
        j->deadline = deadline_in(EXEC_EOF_GRACE_MS);
    }
}

// Service job timers/reaps after each select() wake. Non-blocking: every check
// is a single WNOHANG (the only blocking reap is the bounded one after we have
// already SIGKILLed). Handles: an EOF'd exec awaiting its exit (reap when done,
// or force-kill a detached child once its grace elapses); a wait timer; and an
// exec past its run timeout.
static void expire_jobs(int fd) {
    for (int i = 0; i < MAX_JOBS; i++) {
        struct job *j = &g_jobs[i];
        if (!j->active) continue;
        if (j->eof) {
            int wstatus = 0;
            if (try_reap(j->pid, &wstatus)) {
                deliver_exec(fd, j, exit_code_of(wstatus));
            } else if (ms_until(&j->deadline) <= 0) {
                // Grace elapsed, still alive → detached. Kill the whole session
                // group (the child setsid()'d, so -pid reaches backgrounded
                // grandchildren too), reap, report null exit.
                kill(-j->pid, SIGKILL);
                reap_pid(j->pid, &wstatus);
                deliver_exec(fd, j, -1);
            }
            continue;
        }
        if (ms_until(&j->deadline) > 0) continue;
        if (j->is_wait) {
            send_ok(fd, j->req_id);
            j->active = 0;
        } else {
            // exec run-timeout: kill the whole session group, reap, null exit.
            int wstatus = 0;
            kill(-j->pid, SIGKILL);
            reap_pid(j->pid, &wstatus);
            deliver_exec(fd, j, -1);
        }
    }
}

// select() timeout = time until the nearest job deadline, or NULL (block) when
// no jobs are pending. Negative remaining clamps to 0 (fire immediately).
static struct timeval *jobs_timeout(struct timeval *tv) {
    long min_ms = -1;
    for (int i = 0; i < MAX_JOBS; i++) {
        if (!g_jobs[i].active) continue;
        // An EOF'd exec is polled for its exit on a short fixed interval; every
        // other job wakes at its deadline.
        long ms = g_jobs[i].eof ? EXEC_EOF_POLL_MS : ms_until(&g_jobs[i].deadline);
        if (ms < 0) ms = 0;
        if (min_ms < 0 || ms < min_ms) min_ms = ms;
    }
    if (min_ms < 0) return NULL;
    tv->tv_sec = min_ms / 1000;
    tv->tv_usec = (min_ms % 1000) * 1000;
    return tv;
}

// Kill + reap any still-running exec jobs at teardown so children don't leak.
static void reap_all_jobs(void) {
    for (int i = 0; i < MAX_JOBS; i++) {
        struct job *j = &g_jobs[i];
        if (!j->active || j->is_wait) continue;
        kill(-j->pid, SIGKILL); // whole session group, not just sh
        reap_pid(j->pid, NULL);
        if (j->outfd >= 0) close(j->outfd); // -1 once the job is eof-awaiting
        free(j->buf);
        j->active = 0;
    }
}

// Dispatch one request. Most actions reply synchronously and return the
// send_*() result (<0 ⇒ host gone). `exec_capture` and `wait` instead register
// an async job and return 0 — their reply is sent later by the main loop.
static int handle(int fd, uint32_t req_id, const char *json) {
    char action[64];
    if (!json_str(json, "action", action, sizeof(action)))
        return send_err(fd, req_id,"missing action");

    long x = 0, y = 0, amount = 0, ms = 0;

    if (!strcmp(action, "screenshot")) {
        return do_screenshot(fd, req_id);
    } else if (!strcmp(action, "cursor_position")) {
        Window r, c; int rx, ry, wx, wy; unsigned int mask;
        if (!XQueryPointer(g_dpy, g_root, &r, &c, &rx, &ry, &wx, &wy, &mask))
            return send_err(fd, req_id,"XQueryPointer failed");
        return send_coords(fd, req_id, rx, ry);
    } else if (!strcmp(action, "mouse_move")) {
        json_int(json, "x", &x); json_int(json, "y", &y);
        move_pointer((int)x, (int)y);
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "left_click")) {
        click_button(1); return send_ok(fd, req_id);
    } else if (!strcmp(action, "right_click")) {
        click_button(3); return send_ok(fd, req_id);
    } else if (!strcmp(action, "middle_click")) {
        click_button(2); return send_ok(fd, req_id);
    } else if (!strcmp(action, "double_click")) {
        click_button(1); usleep(50000); click_button(1);
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "left_click_drag")) {
        json_int(json, "x", &x); json_int(json, "y", &y);
        XTestFakeButtonEvent(g_dpy, 1, True, CurrentTime);
        XFlush(g_dpy);
        move_pointer((int)x, (int)y);
        XTestFakeButtonEvent(g_dpy, 1, False, CurrentTime);
        XFlush(g_dpy);
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "type")) {
        char text[8192];
        if (!json_str(json, "text", text, sizeof(text)))
            return send_err(fd, req_id,"missing text");
        if (do_type(text) < 0) return send_err(fd, req_id,"no scratch keycode");
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "key")) {
        char keys[256];
        if (!json_str(json, "keys", keys, sizeof(keys)))
            return send_err(fd, req_id,"missing keys");
        if (do_key_chord(keys) < 0) return send_err(fd, req_id,"bad key chord");
        return send_ok(fd, req_id);
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
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "wait")) {
        json_int(json, "ms", &ms);
        if (ms <= 0) return send_ok(fd, req_id);
        // Async timer: reply ok when the deadline fires, without blocking the
        // loop. The reply carries this req_id; the host demuxes it.
        start_wait(req_id, ms);
        return 0;
    } else if (!strcmp(action, "exec")) {
        char cmd[4096];
        if (!json_str(json, "command", cmd, sizeof(cmd)))
            return send_err(fd, req_id,"missing command");
        launch_detached(cmd);
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "navigate")) {
        // Size the URL buffer to the header; a long URL must not be truncated.
        size_t cap = strlen(json) + 1;
        char *url = (char *)malloc(cap);
        if (!url) return send_err(fd, req_id,"oom");
        if (!json_str(json, "url", url, cap)) {
            free(url);
            return send_err(fd, req_id,"missing url");
        }
        // Hand the URL to the launcher as one argv element — no shell, so it
        // can't be word-split or used to inject commands.
        launch_argv("vmette-open", url);
        free(url);
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "exec_capture")) {
        char cmd[4096];
        if (!json_str(json, "command", cmd, sizeof(cmd)))
            return send_err(fd, req_id,"missing command");
        long timeout_ms = 0;
        json_int(json, "timeout_ms", &timeout_ms);
        // Default and clamp below the host's per-request vsock timeout (30s),
        // or a long command would trip that before the reply is sent.
        if (timeout_ms <= 0) timeout_ms = 15000;
        if (timeout_ms > 25000) timeout_ms = 25000;
        // Async: spawn the child and reply later (on its EOF or the deadline),
        // so a long command no longer stalls input/screenshots/clipboard.
        start_exec_capture(fd, req_id, cmd, timeout_ms);
        return 0;
    } else if (!strcmp(action, "set_clipboard")) {
        // The decoded text can't exceed the header length; size the buffer to
        // it so large pastes aren't truncated by a fixed cap.
        size_t cap = strlen(json) + 1;
        char *text = (char *)malloc(cap);
        if (!text) return send_err(fd, req_id,"oom");
        if (!json_str(json, "text", text, cap)) {
            free(text);
            return send_err(fd, req_id,"missing text");
        }
        do_set_clipboard(text, strlen(text)); // takes ownership of `text`
        return send_ok(fd, req_id);
    } else if (!strcmp(action, "get_clipboard")) {
        return do_get_clipboard(fd, req_id);
    }
    return send_err(fd, req_id,"unknown action");
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

    // Unmapped 1x1 window that owns the clipboard selections; intern the atoms
    // serve_selection()/do_get_clipboard() need. (XA_PRIMARY/XA_STRING/XA_ATOM
    // are predefined.)
    g_clip_win = XCreateSimpleWindow(g_dpy, g_root, 0, 0, 1, 1, 0, 0, 0);
    A_CLIPBOARD = XInternAtom(g_dpy, "CLIPBOARD", False);
    A_TARGETS = XInternAtom(g_dpy, "TARGETS", False);
    A_UTF8 = XInternAtom(g_dpy, "UTF8_STRING", False);
    A_TEXT = XInternAtom(g_dpy, "TEXT", False);
    A_INCR = XInternAtom(g_dpy, "INCR", False);
    A_PROP = XInternAtom(g_dpy, "VMETTE_CLIP", False); // scratch prop for get

    // Disable keyboard auto-repeat globally. If a synthetic key's press/release
    // pair straddles a slow round-trip, the server can inject spurious repeat
    // KeyPress events and typed text comes out with doubled characters. We never
    // want auto-repeat on injected input.
    XAutoRepeatOff(g_dpy);
    XSync(g_dpy, False);

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

    // Multiplex the vsock request stream, the X connection, and any in-flight
    // async-job pipes (exec_capture). Requests are served as they arrive;
    // SelectionRequest events (an app pasting what set_clipboard owns) are
    // served via serve_selection; exec/wait jobs complete in the background and
    // reply out of order. The select() timeout tracks the nearest job deadline
    // so exec timeouts and wait timers fire on schedule.
    int xfd = ConnectionNumber(g_dpy);
    for (;;) {
        // Serve any X events already queued before blocking.
        while (XPending(g_dpy)) {
            XEvent ev;
            XNextEvent(g_dpy, &ev);
            serve_selection(&ev);
        }

        fd_set rfds;
        FD_ZERO(&rfds);
        FD_SET(fd, &rfds);
        FD_SET(xfd, &rfds);
        int maxfd = (fd > xfd ? fd : xfd);
        for (int i = 0; i < MAX_JOBS; i++) {
            // Only exec jobs with a live pipe (not yet EOF'd → outfd >= 0) are
            // selectable; an eof-awaiting job has no fd, it polls via jobs_timeout.
            if (g_jobs[i].active && !g_jobs[i].is_wait && g_jobs[i].outfd >= 0) {
                FD_SET(g_jobs[i].outfd, &rfds);
                if (g_jobs[i].outfd > maxfd) maxfd = g_jobs[i].outfd;
            }
        }
        struct timeval tvbuf;
        struct timeval *tv = jobs_timeout(&tvbuf);
        if (select(maxfd + 1, &rfds, NULL, NULL, tv) < 0) {
            if (errno == EINTR) continue;
            break;
        }

        // Deadlines/reaps first (a wait fires; an EOF'd exec is reaped; an exec
        // past its timeout is killed), then drain any exec pipes with output/EOF.
        expire_jobs(fd);
        for (int i = 0; i < MAX_JOBS; i++) {
            if (g_jobs[i].active && !g_jobs[i].is_wait && g_jobs[i].outfd >= 0 &&
                FD_ISSET(g_jobs[i].outfd, &rfds))
                drain_job(fd, &g_jobs[i]);
        }

        if (FD_ISSET(xfd, &rfds)) {
            while (XPending(g_dpy)) {
                XEvent ev;
                XNextEvent(g_dpy, &ev);
                serve_selection(&ev);
            }
        }

        if (FD_ISSET(fd, &rfds)) {
            // Frame prefix: [u32 LE req_id][u32 LE header_len]. The req_id is a
            // local passed to handle(), so a deferred async reply later echoes
            // the id captured here even after newer requests are served.
            unsigned char prefix[8];
            if (read_exact(fd, prefix, 8) < 0) break; // host closed
            uint32_t req_id = (uint32_t)prefix[0] | ((uint32_t)prefix[1] << 8) |
                              ((uint32_t)prefix[2] << 16) |
                              ((uint32_t)prefix[3] << 24);
            uint32_t hlen = (uint32_t)prefix[4] | ((uint32_t)prefix[5] << 8) |
                            ((uint32_t)prefix[6] << 16) |
                            ((uint32_t)prefix[7] << 24);
            if (hlen == 0 || hlen > (1u << 20)) break;
            char *hdr = (char *)malloc(hlen + 1);
            if (!hdr) break;
            if (read_exact(fd, hdr, hlen) < 0) { free(hdr); break; }
            hdr[hlen] = '\0';
            int rc = handle(fd, req_id, hdr);
            free(hdr);
            if (rc < 0) break; // write failed → host gone
        }
    }

    reap_all_jobs(); // kill + reap any children still running at teardown
    close(fd);
    XCloseDisplay(g_dpy);
    return 0;
}
