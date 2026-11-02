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
