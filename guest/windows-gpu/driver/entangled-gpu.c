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
#define CMD_HALF 2048

#define VIRTIO_GPU_CMD_GET_CAPSET_INFO 0x0108
#define VIRTIO_GPU_RESP_OK_CAPSET_INFO 0x1102

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
#pragma pack(pop)

typedef struct {
    PHYSICAL_ADDRESS Base; /* from the BAR register */
    ULONG Length;
    PUCHAR Va;             /* mapped, or NULL */
} BAR;

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

    UINT64 DeviceFeatures;
    UINT64 DriverFeatures;
    BOOLEAN Ready;
} DEVICE_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DEVICE_CONTEXT, GetCtx)

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

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DEVICE_CONTEXT);
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
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

    /* Map every memory BAR up to 1 MiB: the register BAR. The 4 GiB
     * shared-memory BAR is mapped per blob later, never whole. */
    for (i = 0; i < WdfCmResourceListGetCount(Translated); i++) {
        PCM_PARTIAL_RESOURCE_DESCRIPTOR raw = WdfCmResourceListGetDescriptor(Raw, i);
        PCM_PARTIAL_RESOURCE_DESCRIPTOR tr = WdfCmResourceListGetDescriptor(Translated, i);
        ULONG b;
        if (raw->Type != CmResourceTypeMemory || tr->Type != CmResourceTypeMemory) {
            continue;
        }
        if (tr->u.Memory.Length == 0 || tr->u.Memory.Length > 0x100000) {
            continue;
        }
        for (b = 0; b < 6; b++) {
            if (ctx->Bars[b].Base.QuadPart == raw->u.Memory.Start.QuadPart) {
                ctx->Bars[b].Length = tr->u.Memory.Length;
                ctx->Bars[b].Va = MmMapIoSpaceEx(tr->u.Memory.Start, tr->u.Memory.Length,
                                                 PAGE_READWRITE | PAGE_NOCACHE);
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
        ctx->Cmd = MmAllocateContiguousMemorySpecifyCache(PAGE_SIZE, low, high, boundary,
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

    if (!ctx->Ready || request > CMD_HALF || answer > CMD_HALF) {
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
    for (tries = 0; tries < 2000; tries++) {
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

VOID EgEvtIoDeviceControl(WDFQUEUE Queue, WDFREQUEST Request, size_t OutputLength,
                          size_t InputLength, ULONG IoControlCode)
{
    DEVICE_CONTEXT *ctx = GetCtx(WdfIoQueueGetDevice(Queue));
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR written = 0;
    UNREFERENCED_PARAMETER(OutputLength);
    UNREFERENCED_PARAMETER(InputLength);

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
    default:
        break;
    }
    WdfRequestCompleteWithInformation(Request, status, written);
}
