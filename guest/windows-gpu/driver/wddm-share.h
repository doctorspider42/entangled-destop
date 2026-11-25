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
#define ENTANGLED_WDDM_ACCESS_BLOB 0x907
#define ENTANGLED_WDDM_SHARE_VERSION 1

typedef struct _ENTANGLED_WDDM_BLOB_ESCAPE {
    unsigned short Type;
    unsigned short Version;
    unsigned long Allocation; /* caller's DXGK allocation handle */
    unsigned long long Token;  /* bind input, or open output owned by caller */
} ENTANGLED_WDDM_BLOB_ESCAPE;

#define ENTANGLED_WDDM_BEGIN_WRITE 1
#define ENTANGLED_WDDM_BEGIN_READ 2
#define ENTANGLED_WDDM_END_ACCESS 3
#define ENTANGLED_WDDM_ABORT_ACCESS 4
#define ENTANGLED_WDDM_BEGIN_WRITE_AFTER_READ 5
#define ENTANGLED_WDDM_DISABLE_GPU_ACCESS 6
#define ENTANGLED_WDDM_BEGIN_WRITE_WITH_CPU_SNAPSHOT 7

/* Access is nonblocking. The calling thread owns the lease on successful
 * BEGIN and finishes it only after its GPU fence and external ownership
 * release. Successful END publishes a new generation for a writer only.
 * BEGIN_READ passes the last copied generation and returns the latest one
 * on success. STATUS_NO_MORE_ENTRIES means the image is already current;
 * STATUS_RETRY means that another thread owns the lease. An abandoned or
 * aborted lease disables GPU access to the publication for its lifetime.
 * BEGIN_WRITE_AFTER_READ requires one previously completed GPU read.
 * BEGIN_WRITE_WITH_CPU_SNAPSHOT requires the writer to update the CPU
 * snapshot before END. A later CPU write outside that lease revokes access.
 * A producer can disable further GPU access when reverting to CPU pixels. */
typedef struct _ENTANGLED_WDDM_BLOB_ACCESS_ESCAPE {
    unsigned short Type;
    unsigned short Version;
    unsigned long Allocation;
    unsigned long Operation;
    unsigned long Reserved;
    unsigned long long Generation;
} ENTANGLED_WDDM_BLOB_ACCESS_ESCAPE;
