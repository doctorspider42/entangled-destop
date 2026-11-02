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

/* The most one SUBMIT_3D carries through this slice's command page. */
#define ENTANGLED_GPU_MAX_SUBMIT 1024

typedef struct _ENTANGLED_GPU_CTX_CREATE {
    unsigned long CtxId;
    unsigned long CapsetId; /* 4 = Venus */
} ENTANGLED_GPU_CTX_CREATE;

typedef struct _ENTANGLED_GPU_CTX {
    unsigned long CtxId;
} ENTANGLED_GPU_CTX;

typedef struct _ENTANGLED_GPU_SUBMIT {
    unsigned long CtxId;
    unsigned long Size;
    /* followed by Size bytes of command stream */
} ENTANGLED_GPU_SUBMIT;
