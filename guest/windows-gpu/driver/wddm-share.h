/* Private allocation-sharing contract between the Windows display and render
 * miniports, and the display UMD. Both layouts are fixed for WOW64. */
#pragma once

#define ENTANGLED_GPU_KERNEL_NAME L"\\GLOBAL??\\EntangledGpu"
#define IOCTL_ENTANGLED_GPU_PIN_WDDM_BLOB \
    CTL_CODE(ENTANGLED_GPU_TYPE, 0x80E, METHOD_BUFFERED, FILE_ANY_ACCESS)
/* Kernel mode only. In BLOB_TOKEN (a kernel handle to the event). A new
 * render-device file holds the pin until the display miniport closes it. */

#define ENTANGLED_WDDM_BIND_BLOB 0x905
#define ENTANGLED_WDDM_OPEN_BLOB 0x906
#define ENTANGLED_WDDM_SHARE_VERSION 1

typedef struct _ENTANGLED_WDDM_BLOB_ESCAPE {
    unsigned short Type;
    unsigned short Version;
    unsigned long Allocation; /* caller's DXGK allocation handle */
    unsigned long long Token;  /* bind input, or open output owned by caller */
} ENTANGLED_WDDM_BLOB_ESCAPE;
