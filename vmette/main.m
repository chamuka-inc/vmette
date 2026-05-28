// vmette — full-wire Virtualization.framework Linux sandbox driver.
//
// Wires up:
//   * direct Linux kernel boot (VZLinuxBootLoader)
//   * virtio-console → host stdin/stdout
//   * virtio-fs root share (host dir as guest /, rw or ro)
//   * virtio-fs extra shares (--share TAG=PATH, repeatable)
//   * virtio-block disks (--disk PATH, repeatable)
//   * vsock device with a host-side listener (auto-allocated port by default)
//   * vmette.exec=<base64(cmd)> on kernel cmdline → custom /init runs it
//   * exit-code propagation: guest writes /.vmette-exit, host reads it
//   * --timeout for bounded runs (exit 124 if guest doesn't poweroff in time)
//   * --switch-root: cleaner PID-1 environment than chroot
//
// Process exits when the guest powers off (delegate fires).

#import <Foundation/Foundation.h>
#import <Virtualization/Virtualization.h>
#include <termios.h>
#include <unistd.h>
#include <signal.h>
#include <stdlib.h>
#include <stdio.h>
#include <stdbool.h>
#include <string.h>
#include <time.h>

// ---- terminal raw mode --------------------------------------------------

static struct termios g_saved_termios;
static bool g_termios_saved = false;

static void enter_raw_mode(void) {
    if (!isatty(STDIN_FILENO)) return;
    struct termios t;
    if (tcgetattr(STDIN_FILENO, &t) != 0) return;
    g_saved_termios = t;
    g_termios_saved = true;
    cfmakeraw(&t);
    tcsetattr(STDIN_FILENO, TCSANOW, &t);
}

static void restore_terminal(void) {
    if (g_termios_saved) {
        tcsetattr(STDIN_FILENO, TCSANOW, &g_saved_termios);
        g_termios_saved = false;
    }
}

static void on_signal(int sig) {
    restore_terminal();
    _exit(128 + sig);
}

// ---- vsock listener delegate -------------------------------------------

@interface VsockLogger : NSObject <VZVirtioSocketListenerDelegate>
@property (assign) uint32_t port;
// One-shot callback fired when the guest writes "READY\n" to this port.
// Used by snapshot build mode to trigger pause+save.
@property (copy, nullable) void (^readyHandler)(void);
@end

@implementation VsockLogger
- (BOOL)listener:(VZVirtioSocketListener *)listener
  shouldAcceptNewConnection:(VZVirtioSocketConnection *)connection
           fromSocketDevice:(VZVirtioSocketDevice *)socketDevice {
    int fd = dup([connection fileDescriptor]);
    fprintf(stderr, "\r\n[vsock] guest connected on port %u (fd=%d)\r\n",
            self.port, fd);
    __weak VsockLogger *weakSelf = self;
    dispatch_queue_t q = dispatch_get_global_queue(QOS_CLASS_UTILITY, 0);
    dispatch_async(q, ^{
        char buf[4096];
        ssize_t n;
        while ((n = read(fd, buf, sizeof(buf))) > 0) {
            // READY detection for snapshot build mode (one-shot).
            VsockLogger *me = weakSelf;
            if (me && me.readyHandler && memmem(buf, (size_t)n, "READY\n", 6)) {
                void (^h)(void) = me.readyHandler;
                me.readyHandler = nil;
                dispatch_async(dispatch_get_main_queue(), ^{ h(); });
            }
            fprintf(stderr, "[vsock %u] ", self.port);
            fwrite(buf, 1, (size_t)n, stderr);
            if (buf[n-1] != '\n') fputc('\n', stderr);
            // Echo back so the guest's caller sees a response and unblocks.
            ssize_t off = 0;
            while (off < n) {
                ssize_t w = write(fd, buf + off, (size_t)(n - off));
                if (w < 0) break;
                off += w;
            }
        }
        close(fd);
        fprintf(stderr, "[vsock %u] EOF\r\n", self.port);
    });
    return YES;
}
@end

// ---- VM lifecycle delegate ---------------------------------------------

// Set by the --timeout handler so the VM delegate knows to exit 124
// instead of reading the (potentially missing) exit-code file.
static volatile sig_atomic_t g_timed_out = 0;

@interface VzDelegate : NSObject <VZVirtualMachineDelegate>
// When set, guestDidStop reads this file and exits with its integer
// contents. Lets us propagate the guest's exit code to the host shell.
@property (copy, nullable) NSString *exitCodeFile;
@end

@implementation VzDelegate
- (void)guestDidStopVirtualMachine:(VZVirtualMachine *)vm {
    restore_terminal();
    if (g_timed_out) {
        fprintf(stderr, "\r\n[vmette] guest stopped (timeout, exit 124)\r\n");
        exit(124);
    }
    int code = 0;
    if (self.exitCodeFile) {
        NSError *err = nil;
        NSString *s = [NSString stringWithContentsOfFile:self.exitCodeFile
                                                encoding:NSUTF8StringEncoding
                                                   error:&err];
        if (s) {
            NSString *trimmed = [s stringByTrimmingCharactersInSet:
                [NSCharacterSet whitespaceAndNewlineCharacterSet]];
            code = (int)[trimmed intValue];
            fprintf(stderr, "\r\n[vmette] guest stopped (exit %d)\r\n", code);
        } else {
            fprintf(stderr, "\r\n[vmette] guest stopped (no exit file; assuming 0)\r\n");
        }
    } else {
        fprintf(stderr, "\r\n[vmette] guest stopped\r\n");
    }
    exit(code);
}
- (void)virtualMachine:(VZVirtualMachine *)vm didStopWithError:(NSError *)error {
    restore_terminal();
    fprintf(stderr, "\r\n[vmette] guest stopped with error: %s\r\n",
            [[error localizedDescription] UTF8String] ?: "unknown");
    exit(1);
}
@end

// ---- arg parsing -------------------------------------------------------

typedef struct {
    NSString *kernel;
    NSString *initramfs;
    NSString *cmdline;
    NSString *rootfsShare;
    BOOL      rootfsShareRO;
    NSMutableArray<NSString *> *shares;  // "TAG=PATH"
    NSMutableArray<NSString *> *disks;   // PATH
    NSString *execCmd;                   // raw command string
    BOOL      switchRoot;
    BOOL      net;                       // --net: attach virtio-net + NAT
    int32_t   vsockPort;                 // -1 = disabled, 0 = auto, >0 = explicit
    int       timeoutSeconds;            // 0 = none
    int       vcpus;
    uint64_t  memMiB;
    NSString *buildSnapshot;             // --build-snapshot PATH
    NSString *resumeSnapshot;            // --resume-snapshot PATH
    uint32_t  guestVsockPort;            // --guest-vsock-port (default 1025)
} Args;

__attribute__((noreturn))
static void usage(void) {
    fprintf(stderr,
"vmette --kernel PATH --initramfs PATH [options]\n"
"\n"
"required:\n"
"  --kernel           PATH      bzImage on x86_64\n"
"  --initramfs        PATH      initial ramdisk (use scripts/build-initramfs.sh)\n"
"\n"
"workload:\n"
"  --rootfs-share     PATH      host dir mounted as guest /  via virtio-fs (tag 'rootfs')\n"
"  --ro-rootfs-share            mount rootfs share read-only (default: read-write)\n"
"  --share            TAG=PATH  extra virtio-fs mount; appears at /mnt/<TAG> (repeatable)\n"
"  --disk             PATH      raw block image as virtio-blk device (repeatable)\n"
"  --exec             CMD       shell command to run inside guest, then poweroff\n"
"                               (encoded into kernel cmdline; ~3000 chars max)\n"
"  --net                        attach virtio-net with NAT; /init runs udhcpc on eth0\n"
"  --switch-root                use switch_root instead of chroot for the exec env\n"
"\n"
"runtime:\n"
"  --timeout          N         force-stop the VM after N seconds, exit 124\n"
"  --cmdline          STR       extra kernel cmdline (default 'console=hvc0 quiet')\n"
"  --vsock-port       N         -1=disable vsock entirely; 0=auto-pick in 50000-59999;\n"
"                               >0=explicit. The chosen port is exported to the guest\n"
"                               as VMETTE_VSOCK_PORT.\n"
"  --vcpus            N         default 1\n"
"  --mem-mib          N         default 512\n"
"\n"
"snapshot (macOS 14+):\n"
"  --build-snapshot   PATH      boot, wait for guest READY signal, pause, save to PATH, exit\n"
"  --resume-snapshot  PATH      restore from PATH, send --exec via vsock, drain output\n"
"  --guest-vsock-port N         guest vsock-runner listens on this port (default 1025)\n");
    exit(2);
}

static Args parse_args(int argc, const char **argv) {
    Args a = {
        .cmdline = @"console=hvc0 quiet",
        .vsockPort = 0,           // 0 = auto-allocate
        .vcpus = 1,
        .memMiB = 512,
        .shares = [NSMutableArray array],
        .disks  = [NSMutableArray array],
        .guestVsockPort = 1025,
    };
    for (int i = 1; i < argc; i++) {
        NSString *arg = @(argv[i]);
        #define NEED() do { if (i+1 >= argc) usage(); } while(0)
        if      ([arg isEqualToString:@"--kernel"])           { NEED(); a.kernel       = @(argv[++i]); }
        else if ([arg isEqualToString:@"--initramfs"])        { NEED(); a.initramfs    = @(argv[++i]); }
        else if ([arg isEqualToString:@"--cmdline"])          { NEED(); a.cmdline      = @(argv[++i]); }
        else if ([arg isEqualToString:@"--rootfs-share"])     { NEED(); a.rootfsShare  = @(argv[++i]); }
        else if ([arg isEqualToString:@"--ro-rootfs-share"])  {         a.rootfsShareRO = YES; }
        else if ([arg isEqualToString:@"--share"])            { NEED(); [a.shares addObject:@(argv[++i])]; }
        else if ([arg isEqualToString:@"--disk"])             { NEED(); [a.disks  addObject:@(argv[++i])]; }
        else if ([arg isEqualToString:@"--exec"])             { NEED(); a.execCmd      = @(argv[++i]); }
        else if ([arg isEqualToString:@"--switch-root"])      {         a.switchRoot   = YES; }
        else if ([arg isEqualToString:@"--net"])              {         a.net          = YES; }
        else if ([arg isEqualToString:@"--timeout"])          { NEED(); a.timeoutSeconds = atoi(argv[++i]); }
        else if ([arg isEqualToString:@"--vsock-port"])       { NEED(); a.vsockPort    = (int32_t)atoi(argv[++i]); }
        else if ([arg isEqualToString:@"--vcpus"])            { NEED(); a.vcpus        = atoi(argv[++i]); }
        else if ([arg isEqualToString:@"--mem-mib"])          { NEED(); a.memMiB       = strtoull(argv[++i], NULL, 10); }
        else if ([arg isEqualToString:@"--build-snapshot"])   { NEED(); a.buildSnapshot  = @(argv[++i]); }
        else if ([arg isEqualToString:@"--resume-snapshot"])  { NEED(); a.resumeSnapshot = @(argv[++i]); }
        else if ([arg isEqualToString:@"--guest-vsock-port"]) { NEED(); a.guestVsockPort = (uint32_t)atoi(argv[++i]); }
        else if ([arg isEqualToString:@"-h"] || [arg isEqualToString:@"--help"]) usage();
        else { fprintf(stderr, "unknown arg: %s\n", argv[i]); usage(); }
        #undef NEED
    }
    if (!a.kernel)    { fprintf(stderr, "error: --kernel required\n");    usage(); }
    if (!a.initramfs) { fprintf(stderr, "error: --initramfs required\n"); usage(); }
    if (a.buildSnapshot && a.resumeSnapshot) {
        fprintf(stderr, "error: --build-snapshot and --resume-snapshot are mutually exclusive\n");
        usage();
    }
    if (a.resumeSnapshot && !a.execCmd) {
        fprintf(stderr, "error: --resume-snapshot requires --exec\n");
        usage();
    }
    return a;
}

// ---- helpers -----------------------------------------------------------

// Send `cmd` over `fd` and drain the response. Output is buffered, then
// scanned for the trailing "\n__EXIT__:<N>\n" marker produced by
// vsock-runner; body is written to stdout and the process exits with N.
// Runs on a background queue; calls exit() directly when done.
static void send_and_drain_exit(int fd, NSString *cmd) {
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        const char *bytes = [cmd UTF8String];
        size_t len = strlen(bytes);
        size_t off = 0;
        while (off < len) {
            ssize_t w = write(fd, bytes + off, len - off);
            if (w < 0) { if (errno == EINTR) continue; perror("write(cmd)"); break; }
            off += (size_t)w;
        }
        shutdown(fd, SHUT_WR);

        NSMutableData *all = [NSMutableData data];
        char buf[4096];
        ssize_t n;
        while ((n = read(fd, buf, sizeof(buf))) > 0) {
            [all appendBytes:buf length:(NSUInteger)n];
        }
        close(fd);

        int code = 0;
        NSString *body = nil;
        NSString *out = [[NSString alloc] initWithData:all encoding:NSUTF8StringEncoding];
        if (!out) {
            fwrite([all bytes], 1, [all length], stdout);
            fflush(stdout);
        } else {
            NSRange marker = [out rangeOfString:@"\n__EXIT__:" options:NSBackwardsSearch];
            if (marker.location != NSNotFound) {
                NSString *tail = [out substringFromIndex:marker.location + 10];
                NSString *firstLine = [[tail componentsSeparatedByString:@"\n"] firstObject];
                code = [firstLine intValue];
                body = [out substringToIndex:marker.location + 1];
            } else if ([out hasPrefix:@"__EXIT__:"]) {
                // Edge case: cmd produced no output.
                NSString *tail = [out substringFromIndex:9];
                code = [[[tail componentsSeparatedByString:@"\n"] firstObject] intValue];
                body = @"";
            } else {
                body = out;
            }
            const char *bb = [body UTF8String];
            fwrite(bb, 1, strlen(bb), stdout);
            fflush(stdout);
        }
        restore_terminal();
        fprintf(stderr, "[vmette] resume exit %d\r\n", code);
        exit(code);
    });
}

static uint32_t allocate_vsock_port(void) {
    // High range to avoid colliding with anything well-known. AF_VSOCK
    // doesn't share IP's port namespace; this is just our convention.
    static bool seeded = false;
    if (!seeded) { srand((unsigned)time(NULL) ^ (unsigned)getpid()); seeded = true; }
    return 50000u + (uint32_t)(rand() % 10000);
}

static NSString *build_cmdline(Args a) {
    NSMutableString *cl = [a.cmdline mutableCopy];

    if (a.execCmd) {
        NSData *data = [a.execCmd dataUsingEncoding:NSUTF8StringEncoding];
        NSString *b64 = [data base64EncodedStringWithOptions:0];
        [cl appendFormat:@" vmette.exec=%@", b64];
    }
    if (a.rootfsShare) {
        [cl appendString:@" vmette.rootfs=1"];
        if (a.rootfsShareRO) [cl appendString:@" vmette.rootfs_ro=1"];
    }
    for (NSString *s in a.shares) {
        NSRange eq = [s rangeOfString:@"="];
        if (eq.location == NSNotFound) {
            fprintf(stderr, "warning: --share '%s' missing '='; skipped\n", [s UTF8String]);
            continue;
        }
        NSString *tag = [s substringToIndex:eq.location];
        [cl appendFormat:@" vmette.share=%@", tag];
    }
    if (a.switchRoot)  [cl appendString:@" vmette.switch_root=1"];
    if (a.net)         [cl appendString:@" vmette.net=1"];
    if (a.vsockPort > 0) [cl appendFormat:@" vmette.vsock_port=%d", a.vsockPort];
    if (a.buildSnapshot) {
        [cl appendString:@" vmette.snapshot_mode=server"];
        [cl appendFormat:@" vmette.guest_vsock_port=%u", a.guestVsockPort];
    }

    return cl;
}

// ---- main --------------------------------------------------------------

int main(int argc, const char **argv) {
    @autoreleasepool {
        Args args = parse_args(argc, argv);

        // Auto-allocate vsock port if requested.
        if (args.vsockPort == 0) {
            args.vsockPort = (int32_t)allocate_vsock_port();
        }

        NSString *fullCmdline = build_cmdline(args);

        // Unlink any stale exit-code file so we don't read a previous run's value.
        NSString *exitFile = nil;
        if (args.rootfsShare && !args.rootfsShareRO) {
            exitFile = [args.rootfsShare stringByAppendingPathComponent:@".vmette-exit"];
            [[NSFileManager defaultManager] removeItemAtPath:exitFile error:nil];
        }

        fprintf(stderr,
            "[vmette] kernel       %s\n"
            "[vmette] initramfs    %s\n"
            "[vmette] cmdline      %s\n"
            "[vmette] rootfs-share %s%s\n"
            "[vmette] shares       %lu\n"
            "[vmette] disks        %lu\n"
            "[vmette] exec         %s\n"
            "[vmette] vsock-port   %s%d\n"
            "[vmette] switch-root  %s\n"
            "[vmette] net          %s\n"
            "[vmette] timeout      %ds\n"
            "[vmette] vcpus        %d, memMiB %llu\n\n",
            [args.kernel UTF8String],
            [args.initramfs UTF8String],
            [fullCmdline UTF8String],
            args.rootfsShare ? [args.rootfsShare UTF8String] : "(none)",
            args.rootfsShareRO ? " (ro)" : "",
            (unsigned long)args.shares.count,
            (unsigned long)args.disks.count,
            args.execCmd ? [args.execCmd UTF8String] : "(none — interactive)",
            args.vsockPort < 0 ? "(disabled)" : "",
            args.vsockPort < 0 ? 0 : args.vsockPort,
            args.switchRoot ? "yes" : "no",
            args.net ? "yes (NAT)" : "no",
            args.timeoutSeconds,
            args.vcpus, args.memMiB);

        VZVirtualMachineConfiguration *cfg = [[VZVirtualMachineConfiguration alloc] init];

        VZLinuxBootLoader *boot = [[VZLinuxBootLoader alloc] initWithKernelURL:[NSURL fileURLWithPath:args.kernel]];
        boot.initialRamdiskURL = [NSURL fileURLWithPath:args.initramfs];
        boot.commandLine = fullCmdline;
        cfg.bootLoader = boot;

        cfg.CPUCount   = args.vcpus;
        cfg.memorySize = args.memMiB * 1024ull * 1024ull;

        VZVirtioConsoleDeviceSerialPortConfiguration *serial =
            [[VZVirtioConsoleDeviceSerialPortConfiguration alloc] init];
        serial.attachment = [[VZFileHandleSerialPortAttachment alloc]
            initWithFileHandleForReading:[NSFileHandle fileHandleWithStandardInput]
                    fileHandleForWriting:[NSFileHandle fileHandleWithStandardOutput]];
        cfg.serialPorts = @[ serial ];

        cfg.entropyDevices = @[ [[VZVirtioEntropyDeviceConfiguration alloc] init] ];
        cfg.memoryBalloonDevices = @[ [[VZVirtioTraditionalMemoryBalloonDeviceConfiguration alloc] init] ];

        // virtio-fs: rootfs share + extra shares
        NSMutableArray *fsDevices = [NSMutableArray array];
        if (args.rootfsShare) {
            VZVirtioFileSystemDeviceConfiguration *fs =
                [[VZVirtioFileSystemDeviceConfiguration alloc] initWithTag:@"rootfs"];
            VZSharedDirectory *dir = [[VZSharedDirectory alloc]
                initWithURL:[NSURL fileURLWithPath:args.rootfsShare]
                   readOnly:args.rootfsShareRO];
            fs.share = [[VZSingleDirectoryShare alloc] initWithDirectory:dir];
            [fsDevices addObject:fs];
        }
        for (NSString *s in args.shares) {
            NSRange eq = [s rangeOfString:@"="];
            if (eq.location == NSNotFound) continue;
            NSString *tag  = [s substringToIndex:eq.location];
            NSString *path = [s substringFromIndex:eq.location + 1];
            VZVirtioFileSystemDeviceConfiguration *fs =
                [[VZVirtioFileSystemDeviceConfiguration alloc] initWithTag:tag];
            VZSharedDirectory *dir = [[VZSharedDirectory alloc]
                initWithURL:[NSURL fileURLWithPath:path] readOnly:NO];
            fs.share = [[VZSingleDirectoryShare alloc] initWithDirectory:dir];
            [fsDevices addObject:fs];
        }
        cfg.directorySharingDevices = fsDevices;

        // virtio-blk
        NSMutableArray *storage = [NSMutableArray array];
        for (NSString *diskPath in args.disks) {
            NSError *err = nil;
            VZDiskImageStorageDeviceAttachment *att =
                [[VZDiskImageStorageDeviceAttachment alloc]
                    initWithURL:[NSURL fileURLWithPath:diskPath]
                       readOnly:NO
                          error:&err];
            if (!att) {
                fprintf(stderr, "error: disk %s: %s\n",
                        [diskPath UTF8String],
                        [[err localizedDescription] UTF8String] ?: "unknown");
                return 1;
            }
            [storage addObject:[[VZVirtioBlockDeviceConfiguration alloc] initWithAttachment:att]];
        }
        cfg.storageDevices = storage;

        // virtio-net (NAT) — opt-in via --net.
        if (args.net) {
            VZVirtioNetworkDeviceConfiguration *netDev =
                [[VZVirtioNetworkDeviceConfiguration alloc] init];
            netDev.attachment = [[VZNATNetworkDeviceAttachment alloc] init];
            cfg.networkDevices = @[ netDev ];
        }

        // vsock
        VsockLogger *vsockDelegate = nil;
        if (args.vsockPort > 0) {
            cfg.socketDevices = @[ [[VZVirtioSocketDeviceConfiguration alloc] init] ];
            vsockDelegate = [[VsockLogger alloc] init];
            vsockDelegate.port = (uint32_t)args.vsockPort;
        }

        NSError *vErr = nil;
        if (![cfg validateWithError:&vErr]) {
            fprintf(stderr, "error: config invalid: %s\n",
                    [[vErr localizedDescription] UTF8String] ?: "unknown");
            return 1;
        }

        signal(SIGINT,  on_signal);
        signal(SIGTERM, on_signal);
        signal(SIGHUP,  on_signal);
        atexit(restore_terminal);
        enter_raw_mode();

        VZVirtualMachine *vm = [[VZVirtualMachine alloc] initWithConfiguration:cfg];
        VzDelegate *vmDelegate = [[VzDelegate alloc] init];
        vmDelegate.exitCodeFile = exitFile;
        vm.delegate = vmDelegate;

        if (vsockDelegate && vm.socketDevices.count > 0) {
            VZVirtioSocketDevice *sockDev = (VZVirtioSocketDevice *)vm.socketDevices.firstObject;
            VZVirtioSocketListener *listener = [[VZVirtioSocketListener alloc] init];
            listener.delegate = vsockDelegate;
            [sockDev setSocketListener:listener forPort:(uint32_t)args.vsockPort];
        }

        // Timeout: force-stop after N seconds, exit 124.
        if (args.timeoutSeconds > 0) {
            dispatch_after(
                dispatch_time(DISPATCH_TIME_NOW, (int64_t)args.timeoutSeconds * NSEC_PER_SEC),
                dispatch_get_main_queue(), ^{
                    fprintf(stderr, "\r\n[vmette] timeout %ds reached, force-stopping\r\n",
                            args.timeoutSeconds);
                    // Flag is checked by VzDelegate.guestDidStop so we win
                    // the race against the normal "guest stopped" exit(0).
                    g_timed_out = 1;
                    [vm stopWithCompletionHandler:^(NSError *stopErr) {
                        (void)stopErr;
                        // Fallback in case the delegate doesn't fire (it
                        // should — stop triggers guestDidStop).
                        restore_terminal();
                        exit(124);
                    }];
                });
        }

        // Branch: resume snapshot, build snapshot, or normal start.
        // Apple gates VZ save/restore behind `#if defined(__arm64__)` in
        // VZVirtualMachine.h — the API genuinely doesn't exist on Intel
        // Macs. Fail early with a useful message instead of compiling a
        // broken binary.
#if !defined(__arm64__)
        if (args.buildSnapshot || args.resumeSnapshot) {
            restore_terminal();
            fprintf(stderr,
                "error: --build-snapshot / --resume-snapshot require Apple Silicon.\n"
                "       VZ's save/restore API is `#if defined(__arm64__)` in the SDK\n"
                "       (Virtualization.framework/Headers/VZVirtualMachine.h).\n"
                "       The vsock-runner + /init snapshot-mode wiring is preserved\n"
                "       in this repo; it'll work as soon as it runs on an arm64 Mac.\n");
            return 1;
        }
#endif

#if defined(__arm64__)
        if (args.resumeSnapshot) {
            uint32_t gport = args.guestVsockPort;
            NSString *snap = args.resumeSnapshot;
            NSString *cmd  = args.execCmd;
            [vm restoreMachineStateFromURL:[NSURL fileURLWithPath:snap]
                         completionHandler:^(NSError *err) {
                if (err) {
                    restore_terminal();
                    fprintf(stderr, "error: restore failed: %s\n",
                            [[err localizedDescription] UTF8String] ?: "unknown");
                    exit(1);
                }
                [vm resumeWithCompletionHandler:^(NSError *err2) {
                    if (err2) {
                        restore_terminal();
                        fprintf(stderr, "error: resume failed: %s\n",
                                [[err2 localizedDescription] UTF8String] ?: "unknown");
                        exit(1);
                    }
                    if (vm.socketDevices.count == 0) {
                        restore_terminal();
                        fprintf(stderr, "error: restored VM has no vsock device\n");
                        exit(1);
                    }
                    VZVirtioSocketDevice *dev =
                        (VZVirtioSocketDevice *)vm.socketDevices.firstObject;
                    [dev connectToPort:gport completionHandler:^(VZVirtioSocketConnection *conn, NSError *err3) {
                        if (err3 || !conn) {
                            restore_terminal();
                            fprintf(stderr, "error: connect to guest:%u failed: %s\n",
                                    gport,
                                    [[err3 localizedDescription] UTF8String] ?: "no connection");
                            exit(1);
                        }
                        int fd = dup([conn fileDescriptor]);
                        send_and_drain_exit(fd, cmd);
                    }];
                }];
            }];
        } else if (args.buildSnapshot) {
            if (!vsockDelegate) {
                restore_terminal();
                fprintf(stderr, "error: --build-snapshot requires vsock enabled\n");
                return 1;
            }
            NSString *snap = args.buildSnapshot;
            vsockDelegate.readyHandler = ^{
                fprintf(stderr, "[vmette] guest READY — pausing + saving snapshot\n");
                [vm pauseWithCompletionHandler:^(NSError *err) {
                    if (err) {
                        restore_terminal();
                        fprintf(stderr, "error: pause failed: %s\n",
                                [[err localizedDescription] UTF8String] ?: "unknown");
                        exit(1);
                    }
                    NSURL *url = [NSURL fileURLWithPath:snap];
                    [vm saveMachineStateToURL:url completionHandler:^(NSError *err2) {
                        restore_terminal();
                        if (err2) {
                            fprintf(stderr, "error: save failed: %s\n",
                                    [[err2 localizedDescription] UTF8String] ?: "unknown");
                            exit(1);
                        }
                        fprintf(stderr, "[vmette] snapshot saved to %s\n",
                                [snap UTF8String]);
                        exit(0);
                    }];
                }];
            };
            [vm startWithCompletionHandler:^(NSError * _Nullable startErr) {
                if (startErr) {
                    restore_terminal();
                    fprintf(stderr, "error: vm.start failed: %s\n",
                            [[startErr localizedDescription] UTF8String] ?: "unknown");
                    exit(1);
                }
            }];
        } else
#endif
        {
            [vm startWithCompletionHandler:^(NSError * _Nullable startErr) {
                if (startErr) {
                    restore_terminal();
                    fprintf(stderr, "error: vm.start failed: %s\n",
                            [[startErr localizedDescription] UTF8String] ?: "unknown");
                    exit(1);
                }
            }];
        }

        [[NSRunLoop mainRunLoop] run];
    }
    return 0;
}
