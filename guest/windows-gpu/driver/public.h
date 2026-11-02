/*
 * entangled-gpu: the IOCTL interface of our render-only virtio-gpu driver
 * (ADR-0009). Shared by the driver and user mode.
 *
 * The device is \\.\EntangledGpu. Every IOCTL is METHOD_BUFFERED and
 * synchronous; the driver serialises them.
 */
#pragma once

#define ENTANGLED_GPU_DEVICE_NAME L"\\\\.\\EntangledGpu"

#define ENTANGLED_GPU_TYPE 0x8E47u

/* in: ENTANGLED_GPU_CAPSET_INFO_IN, out: ENTANGLED_GPU_CAPSET_INFO_OUT */
#define IOCTL_ENTANGLED_GPU_CAPSET_INFO \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)

/* out: ENTANGLED_GPU_INFO */
#define IOCTL_ENTANGLED_GPU_INFO \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS)

typedef struct _ENTANGLED_GPU_INFO {
    unsigned long long DeviceFeatures; /* what the device offered */
    unsigned long long DriverFeatures; /* what the driver accepted */
    unsigned long NumCapsets;
    unsigned long QueueSize;
} ENTANGLED_GPU_INFO;

typedef struct _ENTANGLED_GPU_CAPSET_INFO_IN {
    unsigned long Index;
} ENTANGLED_GPU_CAPSET_INFO_IN;

typedef struct _ENTANGLED_GPU_CAPSET_INFO_OUT {
    unsigned long Id;         /* 4 = Venus */
    unsigned long MaxVersion;
    unsigned long MaxSize;
} ENTANGLED_GPU_CAPSET_INFO_OUT;

/* Every command below answers with the host's virtio-gpu response type
 * (0x1100 = OK_NODATA; 0x12xx = an error). */
typedef struct _ENTANGLED_GPU_RESPONSE {
    unsigned long Type;
} ENTANGLED_GPU_RESPONSE;

/* in: ENTANGLED_GPU_CTX_CREATE, out: ENTANGLED_GPU_RESPONSE */
#define IOCTL_ENTANGLED_GPU_CTX_CREATE \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)
/* in: ENTANGLED_GPU_CTX, out: ENTANGLED_GPU_RESPONSE */
#define IOCTL_ENTANGLED_GPU_CTX_DESTROY \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS)
/* in: ENTANGLED_GPU_SUBMIT followed by Size bytes, out: ENTANGLED_GPU_RESPONSE */
#define IOCTL_ENTANGLED_GPU_SUBMIT_3D \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x804, METHOD_BUFFERED, FILE_ANY_ACCESS)

/* The most one SUBMIT_3D carries. */
#define ENTANGLED_GPU_MAX_SUBMIT (60 * 1024)

typedef struct _ENTANGLED_GPU_CTX_CREATE {
    unsigned long CtxId;
    unsigned long CapsetId; /* 4 = Venus */
} ENTANGLED_GPU_CTX_CREATE;

typedef struct _ENTANGLED_GPU_CTX {
    unsigned long CtxId;
} ENTANGLED_GPU_CTX;

/* Flags of ENTANGLED_GPU_SUBMIT. */
#define ENTANGLED_GPU_SUBMIT_FENCE 0x1 /* answer only once RingIdx's work is done */

typedef struct _ENTANGLED_GPU_SUBMIT {
    unsigned long CtxId;
    unsigned long Size;
    unsigned long RingIdx; /* the timeline (0: the context's CPU timeline) */
    unsigned long Flags;
    /* followed by Size bytes of command stream */
} ENTANGLED_GPU_SUBMIT;

/* A host-memory blob of a context, mapped into the calling process: in
 * ENTANGLED_GPU_BLOB_IN, out ENTANGLED_GPU_BLOB_OUT. Created HOST3D and
 * mappable, attached to the context, mapped into the shared-memory BAR and
 * from there into the process; unmapped when the handle is closed. */
#define IOCTL_ENTANGLED_GPU_BLOB_CREATE \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x805, METHOD_BUFFERED, FILE_ANY_ACCESS)

typedef struct _ENTANGLED_GPU_BLOB_IN {
    unsigned long CtxId;
    unsigned long ResourceId;
    unsigned long long Size; /* a multiple of 4 KiB */
    unsigned long long BlobId; /* 0: plain shared memory; else a VkDeviceMemory id */
} ENTANGLED_GPU_BLOB_IN;

/* Unmap and release a blob this handle created: in ENTANGLED_GPU_CTX-like
 * { CtxId, ResourceId }, out ENTANGLED_GPU_RESPONSE. */
#define IOCTL_ENTANGLED_GPU_BLOB_DESTROY     CTL_CODE(ENTANGLED_GPU_TYPE, 0x806, METHOD_BUFFERED, FILE_ANY_ACCESS)

typedef struct _ENTANGLED_GPU_BLOB_REF {
    unsigned long CtxId;
    unsigned long ResourceId;
} ENTANGLED_GPU_BLOB_REF;

/* A capset's bytes: in ENTANGLED_GPU_CAPSET_IN, out ENTANGLED_GPU_CAPSET_OUT. */
#define IOCTL_ENTANGLED_GPU_CAPSET     CTL_CODE(ENTANGLED_GPU_TYPE, 0x807, METHOD_BUFFERED, FILE_ANY_ACCESS)

#define ENTANGLED_GPU_MAX_CAPSET 1024

typedef struct _ENTANGLED_GPU_CAPSET_IN {
    unsigned long Id;
    unsigned long Version;
} ENTANGLED_GPU_CAPSET_IN;

typedef struct _ENTANGLED_GPU_CAPSET_OUT {
    unsigned long Response;
    unsigned long Size;
    unsigned char Data[ENTANGLED_GPU_MAX_CAPSET];
} ENTANGLED_GPU_CAPSET_OUT;

typedef struct _ENTANGLED_GPU_BLOB_OUT {
    unsigned long long Address; /* in the calling process */
    unsigned long Response;     /* the first non-OK answer, or 0x1100 */
    unsigned long MapInfo;
} ENTANGLED_GPU_BLOB_OUT;
