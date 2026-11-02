/* gpu-test: asks entangled-gpu.sys what the render-only function offers
 * (ADR-0009 phase A). Prints one line per fact and ends with
 * "GPU-TEST PASS" when a Venus capset (id 4) came back from the host.
 *
 * `gpu-test install <inf>` puts the driver on the render-only function first:
 * a Microsoft-signed driver (viogpudo) outranks a test-signed one there, so
 * pnputil alone leaves the function where it was. */
#include <windows.h>
#include <newdev.h>
#include <stdio.h>
#include "public.h"

#pragma comment(lib, "newdev.lib")

static int install(const wchar_t *inf)
{
    BOOL reboot = FALSE;
    wchar_t full[MAX_PATH];
    if (!GetFullPathNameW(inf, MAX_PATH, full, NULL)) {
        printf("GPU-TEST FAIL path: error %lu\n", GetLastError());
        return 1;
    }
    if (!UpdateDriverForPlugAndPlayDevicesW(NULL, L"PCI\\VEN_1AF4&DEV_1050&SUBSYS_00401AF4&REV_01",
                                            full, INSTALLFLAG_FORCE, &reboot)) {
        printf("GPU-TEST FAIL install: error %lu\n", GetLastError());
        return 1;
    }
    printf("installed%s\n", reboot ? " (reboot needed)" : "");
    return 0;
}

int wmain(int argc, wchar_t **argv)
{
    HANDLE dev;
    ENTANGLED_GPU_INFO info;
    DWORD got = 0, i;
    int venus = 0;
    if (argc == 3 && lstrcmpiW(argv[1], L"install") == 0) {
        return install(argv[2]);
    }
    dev = CreateFileW(ENTANGLED_GPU_DEVICE_NAME, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                      OPEN_EXISTING, 0, NULL);
    if (dev == INVALID_HANDLE_VALUE) {
        printf("GPU-TEST FAIL open: error %lu\n", GetLastError());
        return 1;
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
    printf(venus ? "GPU-TEST PASS\n" : "GPU-TEST FAIL no Venus capset\n");
    CloseHandle(dev);
    return venus ? 0 : 1;
}
