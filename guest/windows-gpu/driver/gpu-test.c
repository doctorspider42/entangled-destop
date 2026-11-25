/* gpu-test: asks entangled-gpu.sys what the render-only function offers
 * (ADR-0009 phase A). Prints one line per fact and ends with
 * "GPU-TEST PASS" when a Venus capset (id 4) came back from the host.
 *
 * `gpu-test install <inf>` puts the driver on the render-only function first:
 * a Microsoft-signed driver (viogpudo) outranks a test-signed one there, so
 * pnputil alone leaves the function where it was. `gpu-test install <inf>
 * <hardware id>` does the same for another function: the display one, for
 * entangled-display (guest/windows-gpu/display).
 * `gpu-test context-cleanup` checks context ownership across handle closes.
 * `gpu-test hold-venus-ring` leaves a private ring waiting for its round
 * trip, to exercise independent clients on the same control queue. */
#include <windows.h>
#include <newdev.h>
#include <stdio.h>
#include "public.h"

#pragma comment(lib, "newdev.lib")

static int install(const wchar_t *inf, const wchar_t *hardware_id)
{
    BOOL reboot = FALSE;
    wchar_t full[MAX_PATH];
    if (!GetFullPathNameW(inf, MAX_PATH, full, NULL)) {
        printf("GPU-TEST FAIL path: error %lu\n", GetLastError());
        return 1;
    }
    if (!UpdateDriverForPlugAndPlayDevicesW(NULL, hardware_id, full, INSTALLFLAG_FORCE, &reboot)) {
        printf("GPU-TEST FAIL install: error %lu\n", GetLastError());
        return 1;
    }
    printf("installed%s\n", reboot ? " (reboot needed)" : "");
    return 0;
}

/* Little-endian stream building, as the Venus wire wants it. */
typedef struct {
    unsigned char b[512];
    unsigned long n;
} STREAM;
static void put32(STREAM *s, unsigned long v)
{
    memcpy(s->b + s->n, &v, 4);
    s->n += 4;
}
static void put64(STREAM *s, unsigned long long v)
{
    memcpy(s->b + s->n, &v, 8);
    s->n += 8;
}

static BOOL call(HANDLE dev, DWORD code, void *in, DWORD inLen, void *out, DWORD outLen)
{
    DWORD got = 0;
    return DeviceIoControl(dev, code, in, inLen, out, outLen, &got, NULL);
}

static int context_cleanup(void)
{
    ENTANGLED_GPU_CTX_CREATE request = {(GetCurrentProcessId() & 0xffffffu) << 8 | 1u, 4};
    // Closing the first handle releases its context. Reopen the same one,
    // as a client restarting its renderer would do.
    for (unsigned i = 0; i < 2; ++i) {
        HANDLE dev = CreateFileW(ENTANGLED_GPU_DEVICE_NAME, GENERIC_READ | GENERIC_WRITE,
                                FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING,
                                0, NULL);
        if (dev == INVALID_HANDLE_VALUE) {
            printf("GPU-TEST FAIL open: error %lu\n", GetLastError());
            return 1;
        }
        ENTANGLED_GPU_RESPONSE created = {0};
        const BOOL ok = call(dev, IOCTL_ENTANGLED_GPU_CTX_CREATE, &request, sizeof(request),
                             &created, sizeof(created));
        CloseHandle(dev);
        if (!ok || created.Type != 0x1100) {
            printf("GPU-TEST FAIL context create on handle %u: answer %#lx\n", i + 1,
                   created.Type);
            return 1;
        }
    }
    printf("GPU-TEST PASS context cleanup (%#lx)\n", request.CtxId);
    return 0;
}

static unsigned long submit_stream(HANDLE dev, unsigned long ctxId, const STREAM *s, BOOL fenced)
{
    struct {
        ENTANGLED_GPU_SUBMIT head;
        unsigned char bytes[512];
    } req;
    ENTANGLED_GPU_RESPONSE resp = {0};
    memset(&req.head, 0, sizeof(req.head));
    req.head.CtxId = ctxId;
    req.head.Size = s->n;
    if (fenced) {
        req.head.Flags = ENTANGLED_GPU_SUBMIT_FENCE;
        req.head.RingIdx = 1;
    }
    memcpy(req.bytes, s->b, s->n);
    if (!call(dev, IOCTL_ENTANGLED_GPU_SUBMIT_3D, &req, sizeof(req.head) + s->n, &resp,
              sizeof(resp))) {
        return 0;
    }
    return resp.Type;
}

static unsigned long submit(HANDLE dev, unsigned long ctxId, const STREAM *s)
{
    return submit_stream(dev, ctxId, s, FALSE);
}

/* `gpu-test venus`: a Venus ring in a blob of our own, and the host's
 * answer to vkEnumerateInstanceVersion read back through a reply stream. */
#define CTX 9
#define RING_RES 0 /* the driver assigns resource ids */
#define REPLY_RES 0
#define RING_SIZE 0x11000ULL
#define RING_BUFFER_OFFSET 0x100
#define RING_BUFFER_SIZE 0x10000

static unsigned long create_ring(HANDLE dev, unsigned long ctx, unsigned long resource)
{
    STREAM s = {0};
    put32(&s, 188); put32(&s, 0);    /* vkCreateRingMESA */
    put64(&s, 1);                    /* ring handle */
    put64(&s, 1);                    /* pCreateInfo */
    put32(&s, 1000384000);           /* VK_STRUCTURE_TYPE_RING_CREATE_INFO_MESA */
    put64(&s, 0);                    /* pNext */
    put32(&s, 0);                    /* flags */
    put32(&s, resource);
    put64(&s, 0); put64(&s, RING_SIZE);
    put64(&s, 1000000);              /* idleTimeout, ns */
    put64(&s, 0); put64(&s, 4); put64(&s, 8); /* head, tail, status */
    put64(&s, RING_BUFFER_OFFSET); put64(&s, RING_BUFFER_SIZE);
    put64(&s, RING_BUFFER_OFFSET + RING_BUFFER_SIZE); put64(&s, 4); /* extra */
    return submit(dev, ctx, &s);
}

static int hold_venus_ring(void)
{
    const unsigned long ctx = ((GetCurrentProcessId() & 0xffffffu) << 8) | 1u;
    HANDLE dev = CreateFileW(ENTANGLED_GPU_DEVICE_NAME, GENERIC_READ | GENERIC_WRITE,
                            FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);
    if (dev == INVALID_HANDLE_VALUE) {
        printf("HELD-RING FAIL open %lu\n", GetLastError());
        return 1;
    }
    ENTANGLED_GPU_CTX_CREATE create = {ctx, 4};
    ENTANGLED_GPU_RESPONSE created = {0};
    ENTANGLED_GPU_BLOB_IN ring_in = {ctx, 0, RING_SIZE};
    ENTANGLED_GPU_BLOB_OUT ring = {0};
    if (!call(dev, IOCTL_ENTANGLED_GPU_CTX_CREATE, &create, sizeof(create), &created,
              sizeof(created)) || created.Type != 0x1100 ||
        !call(dev, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &ring_in, sizeof(ring_in), &ring,
              sizeof(ring)) || ring.Address == 0 ||
        create_ring(dev, ctx, ring.ResourceId) != 0x1100) {
        printf("HELD-RING FAIL setup %lu\n", GetLastError());
        CloseHandle(dev);
        return 1;
    }

    /* A legal ring dependency on a private roundtrip which this test does
     * not signal. The context-stream wait is therefore expected to time out
     * on the host while the independent client exercises the GPU. */
    STREAM blocked = {0};
    put32(&blocked, 252); put32(&blocked, 0); /* vkWaitVirtqueueSeqnoMESA */
    put64(&blocked, 1);
    volatile unsigned char *pages = (volatile unsigned char *)(ULONG_PTR)ring.Address;
    for (unsigned long i = 0; i < blocked.n; i++) {
        pages[RING_BUFFER_OFFSET + i] = blocked.b[i];
    }
    MemoryBarrier();
    *(volatile unsigned long *)(pages + 4) = blocked.n;
    MemoryBarrier();

    STREAM wait = {0};
    put32(&wait, 253); put32(&wait, 0); /* vkWaitRingSeqnoMESA */
    put64(&wait, 1); put64(&wait, blocked.n);
    LARGE_INTEGER clock, begin, end;
    QueryPerformanceFrequency(&clock);
    QueryPerformanceCounter(&begin);
    printf("WAIT_BEGIN pid=%lu ctx=%lu qpc=%lld freq=%lld\n",
           GetCurrentProcessId(), ctx, begin.QuadPart, clock.QuadPart);
    fflush(stdout);
    const unsigned long response = submit_stream(dev, ctx, &wait, TRUE);
    QueryPerformanceCounter(&end);
    const double ms = 1000.0 * (double)(end.QuadPart - begin.QuadPart) / (double)clock.QuadPart;
    printf("WAIT_END response=%#lx qpc=%lld elapsed_ms=%.2f\n", response, end.QuadPart, ms);
    fflush(stdout);
    CloseHandle(dev);
    if (response < 0x1200 || ms < 4000.0 || ms > 8000.0) {
        printf("HELD-RING FAIL expected an in-band host timeout\n");
        return 1;
    }
    printf("HELD-RING PASS\n");
    return 0;
}

static int venus_round_trip(HANDLE dev)
{
    ENTANGLED_GPU_CTX_CREATE create = {CTX, 4};
    ENTANGLED_GPU_RESPONSE created = {0};
    ENTANGLED_GPU_BLOB_IN ringIn = {CTX, RING_RES, RING_SIZE};
    ENTANGLED_GPU_BLOB_IN replyIn = {CTX, REPLY_RES, 0x1000};
    ENTANGLED_GPU_BLOB_OUT ring = {0}, reply = {0};
    STREAM s = {0};
    volatile unsigned char *r;
    unsigned long type, used = 0, i;

    if (!call(dev, IOCTL_ENTANGLED_GPU_CTX_CREATE, &create, sizeof(create), &created,
              sizeof(created)) ||
        created.Type != 0x1100) {
        printf("VENUS FAIL context: error %lu, answer %#lx\n", GetLastError(), created.Type);
        return 1;
    }
    if (!call(dev, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &ringIn, sizeof(ringIn), &ring,
              sizeof(ring)) ||
        ring.Address == 0 ||
        !call(dev, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &replyIn, sizeof(replyIn), &reply,
              sizeof(reply)) ||
        reply.Address == 0) {
        printf("VENUS FAIL blobs: error %lu, answers %#lx %#lx\n", GetLastError(),
               ring.Response, reply.Response);
        return 1;
    }
    printf("ring blob at %#llx (map info %#lx), reply blob at %#llx\n", ring.Address,
           ring.MapInfo, reply.Address);

    type = create_ring(dev, CTX, ring.ResourceId);
    if (type != 0x1100) {
        printf("VENUS FAIL vkCreateRingMESA: answer %#lx\n", type);
        return 1;
    }

    /* Into the ring: the reply stream, then the call. */
    s.n = 0;
    put32(&s, 178); put32(&s, 0);    /* vkSetReplyCommandStreamMESA */
    put64(&s, 1); put32(&s, reply.ResourceId); put64(&s, 0); put64(&s, 0x1000);
    put32(&s, 137); put32(&s, 1);    /* vkEnumerateInstanceVersion, reply */
    put64(&s, 1);                    /* pApiVersion */
    r = (volatile unsigned char *)(ULONG_PTR)ring.Address;
    for (i = 0; i < s.n; i++) {
        r[RING_BUFFER_OFFSET + i] = s.b[i];
    }
    MemoryBarrier();
    *(volatile unsigned long *)(r + 4) = s.n; /* tail */
    MemoryBarrier();
    {
        STREAM n = {0};
        put32(&n, 190); put32(&n, 0); put64(&n, 1); put32(&n, 0); put32(&n, 0);
        submit(dev, CTX, &n); /* vkNotifyRingMESA */
    }
    for (i = 0; i < 300; i++) {
        used = *(volatile unsigned long *)(r + 0);
        if (used == s.n) {
            break;
        }
        Sleep(10);
    }
    printf("ring head %lu of %lu, status %#lx\n", used, s.n, *(volatile unsigned long *)(r + 8));
    if (used != s.n) {
        printf("VENUS FAIL the host did not consume the ring\n");
        return 1;
    }
    {
        volatile unsigned char *p = (volatile unsigned char *)(ULONG_PTR)reply.Address;
        unsigned long cmd = *(volatile unsigned long *)p;
        long result = *(volatile long *)(p + 4);
        unsigned long version = *(volatile unsigned long *)(p + 16);
        printf("reply: command %lu, result %ld, apiVersion %lu.%lu.%lu\n", cmd, result,
               (version >> 22) & 0x7f, (version >> 12) & 0x3ff, version & 0xfff);
        if (cmd != 137 || result != 0 || version == 0) {
            printf("VENUS FAIL reply\n");
            return 1;
        }
    }
    printf("VENUS PASS\n");
    return 0;
}

int wmain(int argc, wchar_t **argv)
{
    HANDLE dev;
    ENTANGLED_GPU_INFO info;
    DWORD got = 0, i;
    int venus = 0;
    if ((argc == 3 || argc == 4) && lstrcmpiW(argv[1], L"install") == 0) {
        return install(argv[2],
                       argc == 4 ? argv[3] : L"PCI\\VEN_1AF4&DEV_1050&SUBSYS_00401AF4&REV_01");
    }
    if (argc == 2 && lstrcmpiW(argv[1], L"context-cleanup") == 0) {
        return context_cleanup();
    }
    if (argc == 2 && lstrcmpiW(argv[1], L"hold-venus-ring") == 0) {
        return hold_venus_ring();
    }
    dev = CreateFileW(ENTANGLED_GPU_DEVICE_NAME, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                      OPEN_EXISTING, 0, NULL);
    if (dev == INVALID_HANDLE_VALUE) {
        printf("GPU-TEST FAIL open: error %lu\n", GetLastError());
        return 1;
    }
    if (argc == 2 && lstrcmpiW(argv[1], L"venus") == 0) {
        int failed = venus_round_trip(dev);
        CloseHandle(dev);
        return failed;
    }
    if (!DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_INFO, NULL, 0, &info, sizeof(info), &got,
                         NULL)) {
        printf("GPU-TEST FAIL info: error %lu\n", GetLastError());
        return 1;
    }
    printf("device features %#llx, accepted %#llx, capsets %lu, queue %lu\n",
           info.DeviceFeatures, info.DriverFeatures, info.NumCapsets, info.QueueSize);
    for (i = 0; i < info.NumCapsets && i < 16; i++) {
        ENTANGLED_GPU_CAPSET_INFO_IN in = {i};
        ENTANGLED_GPU_CAPSET_INFO_OUT out;
        if (!DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_CAPSET_INFO, &in, sizeof(in), &out,
                             sizeof(out), &got, NULL)) {
            printf("GPU-TEST FAIL capset %lu: error %lu\n", i, GetLastError());
            return 1;
        }
        printf("capset %lu: id %lu, version %lu, size %lu\n", i, out.Id, out.MaxVersion,
               out.MaxSize);
        venus |= out.Id == 4;
    }
    if (!venus) {
        printf("GPU-TEST FAIL no Venus capset\n");
        return 1;
    }
    /* A Venus context: created, an empty stream accepted, an unknown
     * opcode refused (a stream with no length field cannot step over it),
     * destroyed. */
    {
        ENTANGLED_GPU_CTX_CREATE create = {7, 4};
        ENTANGLED_GPU_CTX ctx = {7};
        struct {
            ENTANGLED_GPU_SUBMIT head;
            unsigned long words[2];
        } garbage = {{7, 8}, {4242, 0}};
        ENTANGLED_GPU_SUBMIT empty = {7, 0};
        ENTANGLED_GPU_RESPONSE created, accepted, refused, destroyed;
        if (!DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_CTX_CREATE, &create, sizeof(create),
                             &created, sizeof(created), &got, NULL) ||
            !DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_SUBMIT_3D, &empty, sizeof(empty),
                             &accepted, sizeof(accepted), &got, NULL) ||
            !DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_SUBMIT_3D, &garbage, sizeof(garbage),
                             &refused, sizeof(refused), &got, NULL) ||
            !DeviceIoControl(dev, IOCTL_ENTANGLED_GPU_CTX_DESTROY, &ctx, sizeof(ctx),
                             &destroyed, sizeof(destroyed), &got, NULL)) {
            printf("GPU-TEST FAIL context: error %lu\n", GetLastError());
            return 1;
        }
        printf("context create %#lx, empty submit %#lx, unknown opcode %#lx, destroy %#lx\n",
               created.Type, accepted.Type, refused.Type, destroyed.Type);
        if (created.Type != 0x1100 || accepted.Type != 0x1100 || refused.Type == 0x1100 ||
            destroyed.Type != 0x1100) {
            printf("GPU-TEST FAIL context answers\n");
            return 1;
        }
    }
    printf("GPU-TEST PASS\n");
    CloseHandle(dev);
    return 0;
}
