/*
 * entangled-gpu.sys: our render-only virtio-gpu driver (ADR-0009, phase A).
 *
 * A KMDF function driver for the render-only virtio-gpu function the host
 * adds with `[display] render_node = true` (PCI class 0x0380, subsystem
 * 0x40). It is not a display driver: it owns the virtio transport and hands
 * user mode a small IOCTL interface (public.h), which the Mesa Venus winsys
 * of phase B will speak.
 *
 * This first slice brings the device up and carries control-queue commands:
 * the virtio 1.x PCI transport (capabilities, feature negotiation, one split
 * virtqueue), polled rather than interrupt-driven, and the capset query.
 * Polled is deliberate for now: IOCTLs are serialised by a sequential queue,
 * every command waits for its own answer, and the host answers the control
 * queue synchronously.
 *
 * The device is the host's, which this guest trusts; the checks below guard
 * against a misconfigured machine, not a hostile one.
 */
#include <ntddk.h>
#include <wdf.h>
#include <initguid.h>
#include <wdmguid.h>
#include "public.h"

#define VIRTIO_PCI_CAP_VENDOR 0x09
#define VIRTIO_PCI_CAP_COMMON 1
#define VIRTIO_PCI_CAP_NOTIFY 2
#define VIRTIO_PCI_CAP_DEVICE 4

/* virtio_pci_common_cfg */
#define COMMON_DFSELECT 0x00
#define COMMON_DF 0x04
#define COMMON_GFSELECT 0x08
#define COMMON_GF 0x0c
#define COMMON_MSIX 0x10
#define COMMON_STATUS 0x14
#define COMMON_Q_SELECT 0x16
#define COMMON_Q_SIZE 0x18
#define COMMON_Q_MSIX 0x1a
#define COMMON_Q_ENABLE 0x1c
#define COMMON_Q_NOFF 0x1e
#define COMMON_Q_DESC 0x20
#define COMMON_Q_AVAIL 0x28
#define COMMON_Q_USED 0x30

#define STATUS_ACKNOWLEDGE 1
#define STATUS_DRIVER 2
#define STATUS_DRIVER_OK 4
#define STATUS_FEATURES_OK 8

#define VIRTIO_F_VERSION_1 (1ULL << 32)
#define VIRTIO_GPU_F_VIRGL (1ULL << 0)
#define VIRTIO_GPU_F_RESOURCE_BLOB (1ULL << 3)
#define VIRTIO_GPU_F_CONTEXT_INIT (1ULL << 4)

#define VIRTIO_MSI_NO_VECTOR 0xffff

#define VIRTQ_DESC_F_NEXT 1
#define VIRTQ_DESC_F_WRITE 2

#define MAX_QUEUE_SIZE 64
/* Each command: the request in the first half of the page, the answer in
 * the second. */
/* The command area: requests up to 64 KiB at its start, the answer after. */
#define CMD_BYTES (72 * 1024)
#define REQ_MAX (64 * 1024)
#define CMD_HALF REQ_MAX
#define ANSWER_MAX (8 * 1024)
#define VIRTIO_GPU_CMD_GET_CAPSET 0x0109
#define VIRTIO_GPU_CMD_RESOURCE_UNREF 0x0102
#define VIRTIO_GPU_RESP_OK_CAPSET 0x1103
#define VIRTIO_GPU_FLAG_FENCE 0x1
#define VIRTIO_GPU_FLAG_INFO_RING_IDX 0x2

#define VIRTIO_GPU_CMD_GET_CAPSET_INFO 0x0108
#define VIRTIO_GPU_CMD_CTX_CREATE 0x0200
#define VIRTIO_GPU_CMD_CTX_DESTROY 0x0201
#define VIRTIO_GPU_CMD_SUBMIT_3D 0x0207
#define VIRTIO_GPU_RESP_OK_CAPSET_INFO 0x1102
#define VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE 0x0202
#define VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB 0x010c
#define VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB 0x0208
#define VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB 0x0209
#define VIRTIO_GPU_RESP_OK_NODATA 0x1100
#define VIRTIO_GPU_RESP_OK_MAP_INFO 0x1106
#define VIRTIO_GPU_BLOB_MEM_HOST3D 2
#define VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE 1
/* The render-only function's shared-memory BAR (virtio_core's PLAIN
 * placement puts it at BARs 2-3). */
#define SHM_BAR 2
/* ctx_create.context_init: the capset id in the low byte (CONTEXT_INIT). */
#define VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK 0xff

#pragma pack(push, 1)
typedef struct {
    UINT64 Addr;
    UINT32 Len;
    UINT16 Flags;
    UINT16 Next;
} VIRTQ_DESC;

typedef struct {
    UINT32 Type;
    UINT32 Flags;
    UINT64 FenceId;
    UINT32 CtxId;
    UINT8 RingIdx;
    UINT8 Padding[3];
} GPU_CTRL_HDR;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 CapsetIndex;
    UINT32 Padding;
} GPU_GET_CAPSET_INFO;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 CapsetId;
    UINT32 CapsetMaxVersion;
    UINT32 CapsetMaxSize;
    UINT32 Padding;
} GPU_RESP_CAPSET_INFO;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 NameLen;
    UINT32 ContextInit;
    char DebugName[64];
} GPU_CTX_CREATE;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 Size;
    UINT32 Padding;
    /* followed by Size bytes */
} GPU_SUBMIT_3D;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 ResourceId;
    UINT32 BlobMem;
    UINT32 BlobFlags;
    UINT32 NrEntries;
    UINT64 BlobId;
    UINT64 Size;
} GPU_CREATE_BLOB;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 ResourceId;
    UINT32 Padding;
} GPU_CTX_RESOURCE;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 ResourceId;
    UINT32 Padding;
    UINT64 Offset;
} GPU_MAP_BLOB;

typedef struct {
    GPU_CTRL_HDR Hdr;
    UINT32 MapInfo;
    UINT32 Padding;
} GPU_RESP_MAP_INFO;
#pragma pack(pop)

typedef struct {
    PHYSICAL_ADDRESS Base; /* from the BAR register */
    ULONGLONG Length;
    PUCHAR Va;             /* mapped, or NULL */
} BAR;

/* Blobs in the shared-memory BAR at once, all processes together. */
#define MAX_RANGES 4096

typedef struct {
    BAR Bars[6];
    volatile UCHAR *Common;
    volatile UCHAR *NotifyBase;
    ULONG NotifyMultiplier;
    volatile UCHAR *DeviceCfg;

    PUCHAR Ring; /* descriptors, available ring, used ring: one page */
    PHYSICAL_ADDRESS RingPa;
    PUCHAR Cmd;  /* request and answer: one page */
    PHYSICAL_ADDRESS CmdPa;
    USHORT QueueSize;
    USHORT QueueNotifyOff;
    USHORT AvailIdx;
    USHORT LastUsed;
    UINT64 FenceSeq;

    UINT64 DeviceFeatures;
    UINT64 DriverFeatures;
    BOOLEAN Ready;
    /* The blobs placed in the shared-memory BAR, every handle's, sorted by
     * offset: a new blob takes the first gap that fits. */
    struct {
        UINT64 Offset;
        UINT64 Size;
    } Ranges[MAX_RANGES];
    ULONG RangeCount;
    /* The next blob resource id (ENTANGLED_GPU_BLOB_OUT::ResourceId). */
    ULONG NextResource;
    /* One command in flight: the IOCTL queue and the caller-context blob
     * path both take it. */
    WDFWAITLOCK Lock;
} DEVICE_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DEVICE_CONTEXT, GetCtx)

/* Zink alone keeps dozens of host-visible allocations. */
#define MAX_MAPS 1024
/* The blobs a handle mapped into its process, unmapped and released at
 * cleanup (a user mapping outliving its process is a bugcheck, and a blob
 * left on the host keeps its BAR range). */
typedef struct {
    struct {
        PMDL Mdl;
        ULONG CtxId;
        ULONG ResourceId;
        PVOID User;
        PVOID Kernel;
        SIZE_T Size;
        UINT64 Offset;
    } Maps[MAX_MAPS];
    ULONG Count;
    /* The Venus context this handle created, destroyed at cleanup so a
     * process that dies leaves nothing behind on the host; 0 for none. */
    ULONG CtxId;
} FILE_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(FILE_CONTEXT, GetFileCtx)

EVT_WDF_FILE_CLEANUP EgEvtFileCleanup;
EVT_WDF_IO_IN_CALLER_CONTEXT EgEvtIoInCallerContext;

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD EgEvtDeviceAdd;
EVT_WDF_DEVICE_PREPARE_HARDWARE EgEvtPrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE EgEvtReleaseHardware;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL EgEvtIoDeviceControl;

static UINT8 R8(volatile UCHAR *p, ULONG o) { return READ_REGISTER_UCHAR((PUCHAR)(p + o)); }
static UINT16 R16(volatile UCHAR *p, ULONG o) { return READ_REGISTER_USHORT((PUSHORT)(p + o)); }
static UINT32 R32(volatile UCHAR *p, ULONG o) { return READ_REGISTER_ULONG((PULONG)(p + o)); }
static void W8(volatile UCHAR *p, ULONG o, UINT8 v) { WRITE_REGISTER_UCHAR((PUCHAR)(p + o), v); }
static void W16(volatile UCHAR *p, ULONG o, UINT16 v) { WRITE_REGISTER_USHORT((PUSHORT)(p + o), v); }
static void W32(volatile UCHAR *p, ULONG o, UINT32 v) { WRITE_REGISTER_ULONG((PULONG)(p + o), v); }
static void W64(volatile UCHAR *p, ULONG o, UINT64 v)
{
    W32(p, o, (UINT32)v);
    W32(p, o + 4, (UINT32)(v >> 32));
}

NTSTATUS DriverEntry(PDRIVER_OBJECT DriverObject, PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, EgEvtDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config,
                           WDF_NO_HANDLE);
}

NTSTATUS EgEvtDeviceAdd(WDFDRIVER Driver, PWDFDEVICE_INIT DeviceInit)
{
    WDF_PNPPOWER_EVENT_CALLBACKS pnp;
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_IO_QUEUE_CONFIG queue;
    WDFDEVICE device;
    NTSTATUS status;
    DECLARE_CONST_UNICODE_STRING(link, L"\\DosDevices\\EntangledGpu");
    UNREFERENCED_PARAMETER(Driver);

    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDevicePrepareHardware = EgEvtPrepareHardware;
    pnp.EvtDeviceReleaseHardware = EgEvtReleaseHardware;
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &pnp);
    {
        WDF_FILEOBJECT_CONFIG files;
        WDF_OBJECT_ATTRIBUTES fileAttributes;
        WDF_FILEOBJECT_CONFIG_INIT(&files, WDF_NO_EVENT_CALLBACK, WDF_NO_EVENT_CALLBACK,
                                   EgEvtFileCleanup);
        WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttributes, FILE_CONTEXT);
        WdfDeviceInitSetFileObjectConfig(DeviceInit, &files, &fileAttributes);
    }
    /* Blob mappings go into the caller's address space, so that IOCTL is
     * handled in the caller's context. */
    WdfDeviceInitSetIoInCallerContextCallback(DeviceInit, EgEvtIoInCallerContext);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DEVICE_CONTEXT);
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = WdfWaitLockCreate(WDF_NO_OBJECT_ATTRIBUTES, &GetCtx(device)->Lock);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = WdfDeviceCreateSymbolicLink(device, &link);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    /* Sequential: one IOCTL at a time, so one command in flight. */
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queue, WdfIoQueueDispatchSequential);
    queue.EvtIoDeviceControl = EgEvtIoDeviceControl;
    return WdfIoQueueCreate(device, &queue, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
}

/* The function's BAR base addresses, from its configuration space. */
static NTSTATUS ReadConfig(WDFDEVICE Device, UCHAR Config[256])
{
    BUS_INTERFACE_STANDARD bus;
    NTSTATUS status = WdfFdoQueryForInterface(Device, &GUID_BUS_INTERFACE_STANDARD,
                                              (PINTERFACE)&bus, sizeof(bus), 1, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    ULONG read = bus.GetBusData(bus.Context, PCI_WHICHSPACE_CONFIG, Config, 0, 256);
    bus.InterfaceDereference(bus.Context);
    return read == 256 ? STATUS_SUCCESS : STATUS_DEVICE_CONFIGURATION_ERROR;
}

static ULONG Cfg32(const UCHAR *c, ULONG o) { return *(const ULONG UNALIGNED *)(c + o); }

NTSTATUS EgEvtPrepareHardware(WDFDEVICE Device, WDFCMRESLIST Raw, WDFCMRESLIST Translated)
{
    DEVICE_CONTEXT *ctx = GetCtx(Device);
    UCHAR config[256];
    NTSTATUS status;
    ULONG i;

    status = ReadConfig(Device, config);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    for (i = 0; i < 6; i++) {
        ULONG lo = Cfg32(config, 0x10 + 4 * i);
        if (lo & 1) {
            continue; /* I/O BAR */
        }
        ctx->Bars[i].Base.QuadPart = lo & ~0xfULL;
        if (((lo >> 1) & 3) == 2 && i < 5) {
            ctx->Bars[i].Base.QuadPart |= (ULONGLONG)Cfg32(config, 0x10 + 4 * (i + 1)) << 32;
            i++;
        }
    }

    /* Every memory BAR's length, and a mapping of each up to 1 MiB: the
     * register BAR. The 4 GiB shared-memory BAR (a large-memory resource)
     * is mapped per blob later, never whole. */
    for (i = 0; i < WdfCmResourceListGetCount(Translated); i++) {
        PCM_PARTIAL_RESOURCE_DESCRIPTOR raw = WdfCmResourceListGetDescriptor(Raw, i);
        PCM_PARTIAL_RESOURCE_DESCRIPTOR tr = WdfCmResourceListGetDescriptor(Translated, i);
        ULONG b;
        ULONGLONG length;
        if (raw->Type != CmResourceTypeMemory && raw->Type != CmResourceTypeMemoryLarge) {
            continue;
        }
        length = tr->u.Memory.Length;
        if (tr->Type == CmResourceTypeMemoryLarge) {
            if (tr->Flags & CM_RESOURCE_MEMORY_LARGE_40) {
                length = (ULONGLONG)tr->u.Memory40.Length40 << 8;
            } else if (tr->Flags & CM_RESOURCE_MEMORY_LARGE_48) {
                length = (ULONGLONG)tr->u.Memory48.Length48 << 16;
            } else if (tr->Flags & CM_RESOURCE_MEMORY_LARGE_64) {
                length = (ULONGLONG)tr->u.Memory64.Length64 << 32;
            }
        }
        if (length == 0) {
            continue;
        }
        for (b = 0; b < 6; b++) {
            if (ctx->Bars[b].Base.QuadPart == raw->u.Memory.Start.QuadPart) {
                ctx->Bars[b].Length = length;
                if (length <= 0x100000) {
                    ctx->Bars[b].Va = MmMapIoSpaceEx(tr->u.Memory.Start, (SIZE_T)length,
                                                     PAGE_READWRITE | PAGE_NOCACHE);
                }
            }
        }
    }

    /* The virtio capabilities. */
    if (config[0x06] & 0x10) {
        ULONG at = config[0x34] & 0xfc;
        ULONG guard = 0;
        while (at >= 0x40 && at < 0xfc && guard++ < 48) {
            UCHAR id = config[at], next = config[at + 1];
            if (id == VIRTIO_PCI_CAP_VENDOR && at + 16 <= 256) {
                UCHAR type = config[at + 3], bar = config[at + 4];
                ULONG offset = Cfg32(config, at + 8), length = Cfg32(config, at + 12);
                if (bar < 6 && ctx->Bars[bar].Va != NULL &&
                    offset + length <= ctx->Bars[bar].Length) {
                    volatile UCHAR *va = ctx->Bars[bar].Va + offset;
                    if (type == VIRTIO_PCI_CAP_COMMON) {
                        ctx->Common = va;
                    } else if (type == VIRTIO_PCI_CAP_NOTIFY && at + 20 <= 256) {
                        ctx->NotifyBase = va;
                        ctx->NotifyMultiplier = Cfg32(config, at + 16);
                    } else if (type == VIRTIO_PCI_CAP_DEVICE) {
                        ctx->DeviceCfg = va;
                    }
                }
            }
            at = next & 0xfc;
        }
    }
    if (ctx->Common == NULL || ctx->NotifyBase == NULL || ctx->DeviceCfg == NULL) {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    /* Rings and the command page. */
    {
        PHYSICAL_ADDRESS low = {0}, high, boundary = {0};
        high.QuadPart = -1;
        ctx->Ring = MmAllocateContiguousMemorySpecifyCache(PAGE_SIZE, low, high, boundary,
                                                           MmCached);
        ctx->Cmd = MmAllocateContiguousMemorySpecifyCache(CMD_BYTES, low, high, boundary,
                                                          MmCached);
        if (ctx->Ring == NULL || ctx->Cmd == NULL) {
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        RtlZeroMemory(ctx->Ring, PAGE_SIZE);
        RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
        ctx->RingPa = MmGetPhysicalAddress(ctx->Ring);
        ctx->CmdPa = MmGetPhysicalAddress(ctx->Cmd);
    }

    /* Reset, then the virtio 1.x initialisation sequence. */
    W8(ctx->Common, COMMON_STATUS, 0);
    while (R8(ctx->Common, COMMON_STATUS) != 0) {
        KeStallExecutionProcessor(10);
    }
    /* One bit at a time: the host's transport refuses a skipped step. */
    W8(ctx->Common, COMMON_STATUS, STATUS_ACKNOWLEDGE);
    W8(ctx->Common, COMMON_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    W32(ctx->Common, COMMON_DFSELECT, 0);
    ctx->DeviceFeatures = R32(ctx->Common, COMMON_DF);
    W32(ctx->Common, COMMON_DFSELECT, 1);
    ctx->DeviceFeatures |= (UINT64)R32(ctx->Common, COMMON_DF) << 32;
    ctx->DriverFeatures = ctx->DeviceFeatures & (VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_VIRGL |
                                                 VIRTIO_GPU_F_RESOURCE_BLOB |
                                                 VIRTIO_GPU_F_CONTEXT_INIT);
    if (!(ctx->DriverFeatures & VIRTIO_F_VERSION_1)) {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    W32(ctx->Common, COMMON_GFSELECT, 0);
    W32(ctx->Common, COMMON_GF, (UINT32)ctx->DriverFeatures);
    W32(ctx->Common, COMMON_GFSELECT, 1);
    W32(ctx->Common, COMMON_GF, (UINT32)(ctx->DriverFeatures >> 32));
    W8(ctx->Common, COMMON_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if (!(R8(ctx->Common, COMMON_STATUS) & STATUS_FEATURES_OK)) {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    W16(ctx->Common, COMMON_MSIX, VIRTIO_MSI_NO_VECTOR);

    /* The control queue, queue 0: descriptors at 0, available ring at
     * 1024, used ring at 2048 of the ring page. */
    W16(ctx->Common, COMMON_Q_SELECT, 0);
    ctx->QueueSize = R16(ctx->Common, COMMON_Q_SIZE);
    if (ctx->QueueSize == 0) {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    if (ctx->QueueSize > MAX_QUEUE_SIZE) {
        ctx->QueueSize = MAX_QUEUE_SIZE;
    }
    W16(ctx->Common, COMMON_Q_SIZE, ctx->QueueSize);
    W16(ctx->Common, COMMON_Q_MSIX, VIRTIO_MSI_NO_VECTOR);
    W64(ctx->Common, COMMON_Q_DESC, ctx->RingPa.QuadPart);
    W64(ctx->Common, COMMON_Q_AVAIL, ctx->RingPa.QuadPart + 1024);
    W64(ctx->Common, COMMON_Q_USED, ctx->RingPa.QuadPart + 2048);
    ctx->QueueNotifyOff = R16(ctx->Common, COMMON_Q_NOFF);
    W16(ctx->Common, COMMON_Q_ENABLE, 1);
    W8(ctx->Common, COMMON_STATUS,
       STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    ctx->Ready = TRUE;
    return STATUS_SUCCESS;
}

NTSTATUS EgEvtReleaseHardware(WDFDEVICE Device, WDFCMRESLIST Translated)
{
    DEVICE_CONTEXT *ctx = GetCtx(Device);
    ULONG i;
    UNREFERENCED_PARAMETER(Translated);
    if (ctx->Common != NULL) {
        W8(ctx->Common, COMMON_STATUS, 0); /* reset: the device lets go of the rings */
    }
    ctx->Ready = FALSE;
    for (i = 0; i < 6; i++) {
        if (ctx->Bars[i].Va != NULL) {
            MmUnmapIoSpace(ctx->Bars[i].Va, ctx->Bars[i].Length);
            ctx->Bars[i].Va = NULL;
        }
    }
    if (ctx->Ring != NULL) {
        MmFreeContiguousMemory(ctx->Ring);
        ctx->Ring = NULL;
    }
    if (ctx->Cmd != NULL) {
        MmFreeContiguousMemory(ctx->Cmd);
        ctx->Cmd = NULL;
    }
    return STATUS_SUCCESS;
}

/* One control command: `request` bytes from the command page out, up to
 * `answer` bytes back into its second half. Polled, up to two seconds. */
static NTSTATUS Command(DEVICE_CONTEXT *ctx, ULONG request, ULONG answer)
{
    VIRTQ_DESC *desc = (VIRTQ_DESC *)ctx->Ring;
    volatile UINT16 *avail = (volatile UINT16 *)(ctx->Ring + 1024);
    volatile UINT16 *used = (volatile UINT16 *)(ctx->Ring + 2048);
    LARGE_INTEGER wait;
    ULONG tries;

    if (!ctx->Ready || request > REQ_MAX || answer > ANSWER_MAX) {
        return STATUS_DEVICE_NOT_READY;
    }
    desc[0].Addr = ctx->CmdPa.QuadPart;
    desc[0].Len = request;
    desc[0].Flags = VIRTQ_DESC_F_NEXT;
    desc[0].Next = 1;
    desc[1].Addr = ctx->CmdPa.QuadPart + CMD_HALF;
    desc[1].Len = answer;
    desc[1].Flags = VIRTQ_DESC_F_WRITE;
    desc[1].Next = 0;
    /* avail: flags, idx, ring[] */
    avail[2 + (ctx->AvailIdx % ctx->QueueSize)] = 0;
    KeMemoryBarrier();
    ctx->AvailIdx++;
    avail[1] = ctx->AvailIdx;
    KeMemoryBarrier();
    W16(ctx->NotifyBase, ctx->QueueNotifyOff * ctx->NotifyMultiplier, 0);

    wait.QuadPart = -10 * 1000; /* 1 ms */
    /* Most answers come within microseconds: spin up to ~2 ms first, since
     * a 1 ms sleep really lasts a whole timer tick (~15 ms) on Windows. */
    for (tries = 0; tries < 2000; tries++) {
        KeMemoryBarrier();
        if (used[1] != ctx->LastUsed) {
            ctx->LastUsed = used[1];
            return STATUS_SUCCESS;
        }
        KeStallExecutionProcessor(1);
    }
    /* Then up to ten seconds: a fenced submit answers once the GPU is done. */
    for (tries = 0; tries < 10000; tries++) {
        KeMemoryBarrier();
        if (used[1] != ctx->LastUsed) {
            ctx->LastUsed = used[1];
            return STATUS_SUCCESS;
        }
        KeDelayExecutionThread(KernelMode, FALSE, &wait);
    }
    return STATUS_IO_TIMEOUT;
}

static NTSTATUS CapsetInfo(DEVICE_CONTEXT *ctx, ULONG index, ENTANGLED_GPU_CAPSET_INFO_OUT *out)
{
    GPU_GET_CAPSET_INFO *req = (GPU_GET_CAPSET_INFO *)ctx->Cmd;
    GPU_RESP_CAPSET_INFO *resp = (GPU_RESP_CAPSET_INFO *)(ctx->Cmd + CMD_HALF);
    NTSTATUS status;
    RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
    req->Hdr.Type = VIRTIO_GPU_CMD_GET_CAPSET_INFO;
    req->CapsetIndex = index;
    status = Command(ctx, sizeof(*req), sizeof(*resp));
    if (!NT_SUCCESS(status)) {
        return status;
    }
    if (resp->Hdr.Type != VIRTIO_GPU_RESP_OK_CAPSET_INFO) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    out->Id = resp->CapsetId;
    out->MaxVersion = resp->CapsetMaxVersion;
    out->MaxSize = resp->CapsetMaxSize;
    return STATUS_SUCCESS;
}

/* A command whose answer is only a header: the response type comes back. */
static NTSTATUS HeaderCommand(DEVICE_CONTEXT *ctx, ULONG request, ULONG *type)
{
    NTSTATUS status = Command(ctx, request, sizeof(GPU_CTRL_HDR));
    if (NT_SUCCESS(status)) {
        *type = ((GPU_CTRL_HDR *)(ctx->Cmd + CMD_HALF))->Type;
    }
    return status;
}

static NTSTATUS CtxCreate(DEVICE_CONTEXT *ctx, const ENTANGLED_GPU_CTX_CREATE *in, ULONG *type)
{
    GPU_CTX_CREATE *req = (GPU_CTX_CREATE *)ctx->Cmd;
    static const char name[] = "entangled-gpu";
    RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
    req->Hdr.Type = VIRTIO_GPU_CMD_CTX_CREATE;
    req->Hdr.CtxId = in->CtxId;
    req->NameLen = sizeof(name) - 1;
    RtlCopyMemory(req->DebugName, name, sizeof(name) - 1);
    req->ContextInit = in->CapsetId & VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK;
    return HeaderCommand(ctx, sizeof(*req), type);
}

static NTSTATUS CtxDestroy(DEVICE_CONTEXT *ctx, ULONG ctxId, ULONG *type)
{
    GPU_CTRL_HDR *req = (GPU_CTRL_HDR *)ctx->Cmd;
    RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
    req->Type = VIRTIO_GPU_CMD_CTX_DESTROY;
    req->CtxId = ctxId;
    return HeaderCommand(ctx, sizeof(*req), type);
}

static NTSTATUS Submit3d(DEVICE_CONTEXT *ctx, const ENTANGLED_GPU_SUBMIT *in, size_t inLength,
                         ULONG *type)
{
    GPU_SUBMIT_3D *req = (GPU_SUBMIT_3D *)ctx->Cmd;
    if (in->Size > ENTANGLED_GPU_MAX_SUBMIT || inLength < sizeof(*in) + in->Size) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
    req->Hdr.Type = VIRTIO_GPU_CMD_SUBMIT_3D;
    req->Hdr.CtxId = in->CtxId;
    if (in->Flags & ENTANGLED_GPU_SUBMIT_FENCE) {
        req->Hdr.Flags = VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX;
        req->Hdr.FenceId = ++ctx->FenceSeq;
        req->Hdr.RingIdx = (UINT8)in->RingIdx;
    }
    req->Size = in->Size;
    RtlCopyMemory(req + 1, in + 1, in->Size);
    return HeaderCommand(ctx, sizeof(*req) + in->Size, type);
}

/* A range of the shared-memory BAR for a blob: the first gap that fits.
 * With the lock held. */
static BOOLEAN ShmAlloc(DEVICE_CONTEXT *ctx, UINT64 size, UINT64 *offset)
{
    UINT64 at = 0;
    ULONG i;
    if (ctx->RangeCount >= MAX_RANGES) {
        return FALSE;
    }
    for (i = 0; i <= ctx->RangeCount; i++) {
        UINT64 end = i < ctx->RangeCount ? ctx->Ranges[i].Offset : ctx->Bars[SHM_BAR].Length;
        if (end >= at && end - at >= size) {
            RtlMoveMemory(&ctx->Ranges[i + 1], &ctx->Ranges[i],
                          (ctx->RangeCount - i) * sizeof(ctx->Ranges[0]));
            ctx->Ranges[i].Offset = at;
            ctx->Ranges[i].Size = size;
            ctx->RangeCount++;
            *offset = at;
            return TRUE;
        }
        if (i < ctx->RangeCount) {
            at = ctx->Ranges[i].Offset + ctx->Ranges[i].Size;
        }
    }
    return FALSE;
}

static VOID ShmFree(DEVICE_CONTEXT *ctx, UINT64 offset)
{
    ULONG i;
    for (i = 0; i < ctx->RangeCount; i++) {
        if (ctx->Ranges[i].Offset == offset) {
            RtlMoveMemory(&ctx->Ranges[i], &ctx->Ranges[i + 1],
                          (ctx->RangeCount - i - 1) * sizeof(ctx->Ranges[0]));
            ctx->RangeCount--;
            return;
        }
    }
}

/* Release a blob on the host: unmap it from the BAR (when it was mapped),
 * then drop it. With the lock held. */
static NTSTATUS HostRelease(DEVICE_CONTEXT *ctx, ULONG ctxId, ULONG resourceId, BOOLEAN mapped,
                            ULONG *type)
{
    GPU_CTX_RESOURCE *req = (GPU_CTX_RESOURCE *)ctx->Cmd;
    if (mapped) {
        RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
        req->Hdr.Type = VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB;
        req->Hdr.CtxId = ctxId;
        req->ResourceId = resourceId;
        (void)HeaderCommand(ctx, sizeof(*req), type);
    }
    RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
    req->Hdr.Type = VIRTIO_GPU_CMD_RESOURCE_UNREF;
    req->Hdr.CtxId = ctxId;
    req->ResourceId = resourceId;
    return HeaderCommand(ctx, sizeof(*req), type);
}

/* Create, attach and map one blob, and map its pages into the calling
 * process. Runs in the caller's context, with the lock held. */
static NTSTATUS BlobCreate(DEVICE_CONTEXT *ctx, FILE_CONTEXT *file,
                           const ENTANGLED_GPU_BLOB_IN *in, ENTANGLED_GPU_BLOB_OUT *out)
{
    UINT64 offset;
    ULONG res;
    ULONG type = 0;
    NTSTATUS status;
    PHYSICAL_ADDRESS pa;
    PVOID kernel;
    PMDL mdl;
    PVOID user = NULL;

    out->Address = 0;
    out->MapInfo = 0;
    out->ResourceId = 0;
    out->Response = VIRTIO_GPU_RESP_OK_NODATA;
    if (in->Size == 0 || (in->Size & (PAGE_SIZE - 1)) != 0 || in->Size > 0x10000000 ||
        file->Count >= MAX_MAPS || ctx->Bars[SHM_BAR].Base.QuadPart == 0) {
        return STATUS_INVALID_PARAMETER;
    }
    if (!ShmAlloc(ctx, in->Size, &offset)) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    /* Ours, never 0 and clear of the display function's low ids. */
    if (ctx->NextResource < 0x10000) {
        ctx->NextResource = 0x10000;
    }
    res = ctx->NextResource++;

    {
        GPU_CREATE_BLOB *req = (GPU_CREATE_BLOB *)ctx->Cmd;
        RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
        req->Hdr.Type = VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB;
        req->Hdr.CtxId = in->CtxId;
        req->ResourceId = res;
        req->BlobMem = VIRTIO_GPU_BLOB_MEM_HOST3D;
        req->BlobFlags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE;
        req->BlobId = in->BlobId;
        req->Size = in->Size;
        status = HeaderCommand(ctx, sizeof(*req), &type);
        if (!NT_SUCCESS(status) || type != VIRTIO_GPU_RESP_OK_NODATA) {
            ShmFree(ctx, offset);
            out->Response = type;
            return status;
        }
    }
    {
        GPU_CTX_RESOURCE *req = (GPU_CTX_RESOURCE *)ctx->Cmd;
        RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
        req->Hdr.Type = VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE;
        req->Hdr.CtxId = in->CtxId;
        req->ResourceId = res;
        status = HeaderCommand(ctx, sizeof(*req), &type);
        if (!NT_SUCCESS(status) || type != VIRTIO_GPU_RESP_OK_NODATA) {
            ULONG ignored = 0;
            (void)HostRelease(ctx, in->CtxId, res, FALSE, &ignored);
            ShmFree(ctx, offset);
            out->Response = type;
            return status;
        }
    }
    {
        GPU_MAP_BLOB *req = (GPU_MAP_BLOB *)ctx->Cmd;
        GPU_RESP_MAP_INFO *resp = (GPU_RESP_MAP_INFO *)(ctx->Cmd + CMD_HALF);
        RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
        req->Hdr.Type = VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB;
        req->Hdr.CtxId = in->CtxId;
        req->ResourceId = res;
        req->Offset = offset;
        status = Command(ctx, sizeof(*req), sizeof(*resp));
        if (!NT_SUCCESS(status) || resp->Hdr.Type != VIRTIO_GPU_RESP_OK_MAP_INFO) {
            ULONG ignored = 0;
            if (NT_SUCCESS(status)) {
                out->Response = resp->Hdr.Type;
            }
            (void)HostRelease(ctx, in->CtxId, res, FALSE, &ignored);
            ShmFree(ctx, offset);
            return status;
        }
        out->MapInfo = resp->MapInfo;
    }

    /* Host memory behind a BAR: cached, as the host maps it. */
    pa.QuadPart = ctx->Bars[SHM_BAR].Base.QuadPart + (LONGLONG)offset;
    kernel = MmMapIoSpaceEx(pa, (SIZE_T)in->Size, PAGE_READWRITE);
    mdl = kernel != NULL ? IoAllocateMdl(kernel, (ULONG)in->Size, FALSE, FALSE, NULL) : NULL;
    if (mdl == NULL) {
        ULONG ignored = 0;
        if (kernel != NULL) {
            MmUnmapIoSpace(kernel, (SIZE_T)in->Size);
        }
        (void)HostRelease(ctx, in->CtxId, res, TRUE, &ignored);
        ShmFree(ctx, offset);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    MmBuildMdlForNonPagedPool(mdl);
    __try {
        user = MmMapLockedPagesSpecifyCache(mdl, UserMode, MmCached, NULL, FALSE,
                                            NormalPagePriority | MdlMappingNoExecute);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        user = NULL;
    }
    if (user == NULL) {
        ULONG ignored = 0;
        IoFreeMdl(mdl);
        MmUnmapIoSpace(kernel, (SIZE_T)in->Size);
        (void)HostRelease(ctx, in->CtxId, res, TRUE, &ignored);
        ShmFree(ctx, offset);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    file->Maps[file->Count].Mdl = mdl;
    file->Maps[file->Count].CtxId = in->CtxId;
    file->Maps[file->Count].Offset = offset;
    file->Maps[file->Count].ResourceId = res;
    file->Maps[file->Count].User = user;
    file->Maps[file->Count].Kernel = kernel;
    file->Maps[file->Count].Size = (SIZE_T)in->Size;
    file->Count++;
    out->Address = (ULONG_PTR)user;
    out->ResourceId = res;
    return STATUS_SUCCESS;
}

/* Unmap one of this handle's blobs and release it on the host. */
static NTSTATUS BlobDestroy(DEVICE_CONTEXT *ctx, FILE_CONTEXT *file,
                            const ENTANGLED_GPU_BLOB_REF *in, ULONG *type)
{
    ULONG i;
    BOOLEAN mapped = FALSE;
    for (i = 0; i < file->Count; i++) {
        if (file->Maps[i].ResourceId == in->ResourceId) {
            MmUnmapLockedPages(file->Maps[i].User, file->Maps[i].Mdl);
            IoFreeMdl(file->Maps[i].Mdl);
            MmUnmapIoSpace(file->Maps[i].Kernel, file->Maps[i].Size);
            ShmFree(ctx, file->Maps[i].Offset);
            file->Maps[i] = file->Maps[file->Count - 1];
            file->Count--;
            mapped = TRUE;
            break;
        }
    }
    return HostRelease(ctx, in->CtxId, in->ResourceId, mapped, type);
}

VOID EgEvtIoInCallerContext(WDFDEVICE Device, WDFREQUEST Request)
{
    WDF_REQUEST_PARAMETERS params;
    DEVICE_CONTEXT *ctx = GetCtx(Device);
    ENTANGLED_GPU_BLOB_IN *in = NULL;
    ENTANGLED_GPU_BLOB_OUT *out = NULL;
    NTSTATUS status;

    WDF_REQUEST_PARAMETERS_INIT(&params);
    WdfRequestGetParameters(Request, &params);
    if (params.Type == WdfRequestTypeDeviceControl &&
        params.Parameters.DeviceIoControl.IoControlCode == IOCTL_ENTANGLED_GPU_BLOB_DESTROY) {
        ENTANGLED_GPU_BLOB_REF *ref = NULL;
        ENTANGLED_GPU_RESPONSE *resp = NULL;
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*ref), (PVOID *)&ref, NULL);
        if (NT_SUCCESS(status)) {
            status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*resp), (PVOID *)&resp, NULL);
        }
        if (NT_SUCCESS(status)) {
            ENTANGLED_GPU_BLOB_REF copy = *ref;
            WdfWaitLockAcquire(ctx->Lock, NULL);
            status = BlobDestroy(ctx, GetFileCtx(WdfRequestGetFileObject(Request)), &copy,
                                 &resp->Type);
            WdfWaitLockRelease(ctx->Lock);
        }
        WdfRequestCompleteWithInformation(Request, status,
                                          NT_SUCCESS(status) ? sizeof(*resp) : 0);
        return;
    }
    if (params.Type != WdfRequestTypeDeviceControl ||
        params.Parameters.DeviceIoControl.IoControlCode != IOCTL_ENTANGLED_GPU_BLOB_CREATE) {
        status = WdfDeviceEnqueueRequest(Device, Request);
        if (!NT_SUCCESS(status)) {
            WdfRequestComplete(Request, status);
        }
        return;
    }
    status = WdfRequestRetrieveInputBuffer(Request, sizeof(*in), (PVOID *)&in, NULL);
    if (NT_SUCCESS(status)) {
        status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
    }
    if (NT_SUCCESS(status)) {
        ENTANGLED_GPU_BLOB_IN copy = *in;
        WdfWaitLockAcquire(ctx->Lock, NULL);
        status = BlobCreate(ctx, GetFileCtx(WdfRequestGetFileObject(Request)), &copy, out);
        WdfWaitLockRelease(ctx->Lock);
    }
    WdfRequestCompleteWithInformation(Request, status,
                                      NT_SUCCESS(status) ? sizeof(*out) : 0);
}

VOID EgEvtFileCleanup(WDFFILEOBJECT FileObject)
{
    FILE_CONTEXT *file = GetFileCtx(FileObject);
    ULONG i;
    DEVICE_CONTEXT *ctx = GetCtx(WdfFileObjectGetDevice(FileObject));
    ULONG type = 0;
    /* In the closing process's context, so its user mappings can go; the
     * blobs go on the host too, and with them their BAR ranges. */
    WdfWaitLockAcquire(ctx->Lock, NULL);
    for (i = 0; i < file->Count; i++) {
        MmUnmapLockedPages(file->Maps[i].User, file->Maps[i].Mdl);
        IoFreeMdl(file->Maps[i].Mdl);
        MmUnmapIoSpace(file->Maps[i].Kernel, file->Maps[i].Size);
        ShmFree(ctx, file->Maps[i].Offset);
        (void)HostRelease(ctx, file->Maps[i].CtxId, file->Maps[i].ResourceId, TRUE, &type);
    }
    file->Count = 0;
    if (file->CtxId != 0) {
        (void)CtxDestroy(ctx, file->CtxId, &type);
        file->CtxId = 0;
    }
    WdfWaitLockRelease(ctx->Lock);
}

VOID EgEvtIoDeviceControl(WDFQUEUE Queue, WDFREQUEST Request, size_t OutputLength,
                          size_t InputLength, ULONG IoControlCode)
{
    DEVICE_CONTEXT *ctx = GetCtx(WdfIoQueueGetDevice(Queue));
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR written = 0;
    UNREFERENCED_PARAMETER(OutputLength);
    UNREFERENCED_PARAMETER(InputLength);

    WdfWaitLockAcquire(ctx->Lock, NULL);
    switch (IoControlCode) {
    case IOCTL_ENTANGLED_GPU_INFO: {
        ENTANGLED_GPU_INFO *out = NULL;
        status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
        if (NT_SUCCESS(status)) {
            out->DeviceFeatures = ctx->DeviceFeatures;
            out->DriverFeatures = ctx->DriverFeatures;
            out->NumCapsets = ctx->DeviceCfg != NULL ? R32(ctx->DeviceCfg, 12) : 0;
            out->QueueSize = ctx->QueueSize;
            written = sizeof(*out);
        }
        break;
    }
    case IOCTL_ENTANGLED_GPU_CAPSET_INFO: {
        ENTANGLED_GPU_CAPSET_INFO_IN *in = NULL;
        ENTANGLED_GPU_CAPSET_INFO_OUT *out = NULL;
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*in), (PVOID *)&in, NULL);
        if (NT_SUCCESS(status)) {
            status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
        }
        if (NT_SUCCESS(status)) {
            ULONG index = in->Index;
            status = CapsetInfo(ctx, index, out);
            if (NT_SUCCESS(status)) {
                written = sizeof(*out);
            }
        }
        break;
    }
    case IOCTL_ENTANGLED_GPU_CAPSET: {
        ENTANGLED_GPU_CAPSET_IN *in = NULL;
        ENTANGLED_GPU_CAPSET_OUT *out = NULL;
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*in), (PVOID *)&in, NULL);
        if (NT_SUCCESS(status)) {
            status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
        }
        if (NT_SUCCESS(status)) {
            GPU_CTRL_HDR *req = (GPU_CTRL_HDR *)ctx->Cmd;
            GPU_CTRL_HDR *resp = (GPU_CTRL_HDR *)(ctx->Cmd + CMD_HALF);
            UINT32 *args = (UINT32 *)(req + 1);
            RtlZeroMemory(ctx->Cmd, PAGE_SIZE);
            req->Type = VIRTIO_GPU_CMD_GET_CAPSET;
            args[0] = in->Id;
            args[1] = in->Version;
            status = Command(ctx, sizeof(*req) + 8,
                             sizeof(*resp) + ENTANGLED_GPU_MAX_CAPSET);
            if (NT_SUCCESS(status)) {
                out->Response = resp->Type;
                out->Size = resp->Type == VIRTIO_GPU_RESP_OK_CAPSET ? ENTANGLED_GPU_MAX_CAPSET : 0;
                RtlCopyMemory(out->Data, resp + 1, ENTANGLED_GPU_MAX_CAPSET);
                written = sizeof(*out);
            }
        }
        break;
    }
    case IOCTL_ENTANGLED_GPU_CTX_CREATE:
    case IOCTL_ENTANGLED_GPU_CTX_DESTROY:
    case IOCTL_ENTANGLED_GPU_SUBMIT_3D: {
        PVOID in = NULL;
        size_t inLength = 0;
        ENTANGLED_GPU_RESPONSE *out = NULL;
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(ENTANGLED_GPU_CTX), &in,
                                               &inLength);
        if (NT_SUCCESS(status)) {
            status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
        }
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (IoControlCode == IOCTL_ENTANGLED_GPU_CTX_CREATE) {
            FILE_CONTEXT *file = GetFileCtx(WdfRequestGetFileObject(Request));
            status = inLength >= sizeof(ENTANGLED_GPU_CTX_CREATE) && file->CtxId == 0
                         ? CtxCreate(ctx, (ENTANGLED_GPU_CTX_CREATE *)in, &out->Type)
                         : STATUS_INVALID_PARAMETER;
            if (NT_SUCCESS(status) && out->Type == VIRTIO_GPU_RESP_OK_NODATA) {
                file->CtxId = ((ENTANGLED_GPU_CTX_CREATE *)in)->CtxId;
            }
        } else if (IoControlCode == IOCTL_ENTANGLED_GPU_CTX_DESTROY) {
            FILE_CONTEXT *file = GetFileCtx(WdfRequestGetFileObject(Request));
            status = CtxDestroy(ctx, ((ENTANGLED_GPU_CTX *)in)->CtxId, &out->Type);
            if (file->CtxId == ((ENTANGLED_GPU_CTX *)in)->CtxId) {
                file->CtxId = 0;
            }
        } else {
            status = inLength >= sizeof(ENTANGLED_GPU_SUBMIT)
                         ? Submit3d(ctx, (ENTANGLED_GPU_SUBMIT *)in, inLength, &out->Type)
                         : STATUS_INVALID_PARAMETER;
        }
        if (NT_SUCCESS(status)) {
            written = sizeof(*out);
        }
        break;
    }
    default:
        break;
    }
    WdfWaitLockRelease(ctx->Lock);
    WdfRequestCompleteWithInformation(Request, status, written);
}
