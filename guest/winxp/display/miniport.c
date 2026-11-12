/*
 * entxp.sys — the video miniport of the Entangled display driver for
 * Windows XP (ADR-0013, phase G0).
 *
 * The device is the machine's boot framebuffer (PCI 1234:1111): VRAM in
 * one memory BAR and a page of registers in another, with the Bochs
 * "DISPI" mode registers at offset 0x500 of that page. The miniport's whole
 * job is to tell the display driver which modes exist, set one, and map
 * VRAM; everything that draws is the display driver's (entxp.dll), which
 * lets GDI do it.
 *
 * Freestanding C against videoprt.sys only. Built with clang; see
 * ../build.sh.
 */
#include <dderror.h>
#include <devioctl.h>
#include <miniport.h>
#include <ntddvdeo.h>
#include <video.h>

#include "entxp.h"
#include "../opengl/entgl.h"

/* DISPI register indexes; each is a 16-bit register at DISPI_BASE + 2*n. */
#define DISPI_BASE 0x500
#define DISPI_ID 0
#define DISPI_XRES 1
#define DISPI_YRES 2
#define DISPI_BPP 3
#define DISPI_ENABLE 4
#define DISPI_VIRT_WIDTH 6
#define DISPI_ENABLED 0x01
#define DISPI_LFB 0x40
#define DISPI_NOCLEAR 0x80

#define REGS_LENGTH 0x1000

typedef struct {
    PUCHAR regs;               /* the register page, mapped */
    PHYSICAL_ADDRESS vram;     /* where VRAM is on the bus */
    ULONG vram_length;
    ULONG mode_count;          /* how many of entxp_modes fit in VRAM */
    ULONG current;             /* index of the mode in effect */
    /* The accelerator's transfer area at the top of VRAM (ADR-0013), mapped
     * for good; NULL when the host offers no accelerator. */
    PUCHAR accel_commands;
    ULONG accel_commands_bytes;
    PUCHAR accel_response;
    ULONG accel_response_bytes;
} DEVICE_EXT;

static ULONG accel_read(DEVICE_EXT *ext, ULONG index)
{
    return VideoPortReadRegisterUlong((PULONG)(ext->regs + ACCEL_BASE + 4 * index));
}

/* Finds and maps the transfer area, if the device has an accelerator. */
static void accel_find(PVOID extension, DEVICE_EXT *ext)
{
    ULONG commands, commands_bytes, response, response_bytes;
    PHYSICAL_ADDRESS at;
    PUCHAR base;

    ext->accel_commands = NULL;
    if (accel_read(ext, ACCEL_ID) != ACCEL_MAGIC) {
        return;
    }
    commands = accel_read(ext, ACCEL_COMMANDS);
    commands_bytes = accel_read(ext, ACCEL_COMMANDS_BYTES);
    response = accel_read(ext, ACCEL_RESPONSE);
    response_bytes = accel_read(ext, ACCEL_RESPONSE_BYTES);
    /* One mapping for both: the response area follows the command area. */
    if (commands_bytes == 0 || response_bytes == 0 || response != commands + commands_bytes ||
        commands >= ext->vram_length || commands_bytes + response_bytes > ext->vram_length - commands) {
        return;
    }
    at = ext->vram;
    at.LowPart += commands;
    base = (PUCHAR)VideoPortGetDeviceBase(extension, at, commands_bytes + response_bytes,
                                          VIDEO_MEMORY_SPACE_MEMORY);
    if (base == NULL) {
        return;
    }
    ext->accel_commands = base;
    ext->accel_commands_bytes = commands_bytes;
    ext->accel_response = base + commands_bytes;
    ext->accel_response_bytes = response_bytes;
    /* The modes must leave the area alone. */
    ext->vram_length = commands;
}

static USHORT dispi_read(DEVICE_EXT *ext, ULONG index)
{
    return VideoPortReadRegisterUshort((PUSHORT)(ext->regs + DISPI_BASE + 2 * index));
}

static void dispi_write(DEVICE_EXT *ext, ULONG index, USHORT value)
{
    VideoPortWriteRegisterUshort((PUSHORT)(ext->regs + DISPI_BASE + 2 * index), value);
}

static void set_mode(DEVICE_EXT *ext, ULONG index, BOOLEAN clear)
{
    const ENTXP_MODE *mode = &entxp_modes[index];
    dispi_write(ext, DISPI_ENABLE, 0);
    dispi_write(ext, DISPI_XRES, mode->width);
    dispi_write(ext, DISPI_YRES, mode->height);
    dispi_write(ext, DISPI_BPP, 32);
    dispi_write(ext, DISPI_VIRT_WIDTH, mode->width);
    dispi_write(ext, DISPI_ENABLE,
                (USHORT)(DISPI_ENABLED | DISPI_LFB | (clear ? 0 : DISPI_NOCLEAR)));
    ext->current = index;
}

static void describe(const DEVICE_EXT *ext, ULONG index, VIDEO_MODE_INFORMATION *info)
{
    const ENTXP_MODE *mode = &entxp_modes[index];
    VideoPortZeroMemory(info, sizeof(*info));
    info->Length = sizeof(*info);
    info->ModeIndex = index;
    info->VisScreenWidth = mode->width;
    info->VisScreenHeight = mode->height;
    info->ScreenStride = (ULONG)mode->width * 4;
    info->NumberOfPlanes = 1;
    info->BitsPerPlane = 32;
    info->Frequency = 60;
    info->XMillimeter = 320;
    info->YMillimeter = 240;
    info->NumberRedBits = 8;
    info->NumberGreenBits = 8;
    info->NumberBlueBits = 8;
    info->RedMask = 0x00ff0000;
    info->GreenMask = 0x0000ff00;
    info->BlueMask = 0x000000ff;
    info->AttributeFlags = VIDEO_MODE_COLOR | VIDEO_MODE_GRAPHICS | VIDEO_MODE_NO_OFF_SCREEN;
    info->VideoMemoryBitmapWidth = mode->width;
    info->VideoMemoryBitmapHeight = mode->height;
    (void)ext;
}

VP_STATUS FindAdapter(PVOID extension, PVOID context, PWSTR argument,
                      PVIDEO_PORT_CONFIG_INFO config, PUCHAR again)
{
    DEVICE_EXT *ext = (DEVICE_EXT *)extension;
    VIDEO_ACCESS_RANGE ranges[6];
    VP_STATUS status;
    ULONG i;
    USHORT id;

    (void)context;
    (void)argument;
    *again = FALSE;
    if (config->Length < sizeof(VIDEO_PORT_CONFIG_INFO)) {
        return ERROR_INVALID_PARAMETER;
    }
    VideoPortZeroMemory(ranges, sizeof(ranges));
    status = VideoPortGetAccessRanges(extension, 0, NULL, 6, ranges, NULL, NULL, NULL);
    if (status != NO_ERROR) {
        return status;
    }
    /* Two memory ranges: the register page is the small one, VRAM the big. */
    ext->regs = NULL;
    ext->vram_length = 0;
    for (i = 0; i < 6; i++) {
        if (ranges[i].RangeLength == 0 || ranges[i].RangeInIoSpace) {
            continue;
        }
        if (ranges[i].RangeLength <= 0x10000 && ext->regs == NULL) {
            ext->regs = (PUCHAR)VideoPortGetDeviceBase(extension, ranges[i].RangeStart,
                                                       REGS_LENGTH, VIDEO_MEMORY_SPACE_MEMORY);
        } else if (ranges[i].RangeLength > ext->vram_length) {
            ext->vram = ranges[i].RangeStart;
            ext->vram_length = ranges[i].RangeLength;
        }
    }
    if (ext->regs == NULL || ext->vram_length == 0) {
        return ERROR_DEV_NOT_EXIST;
    }
    /* The device answers with the newest DISPI interface it speaks. */
    id = dispi_read(ext, DISPI_ID);
    if ((id & 0xfff0) != 0xb0c0) {
        return ERROR_DEV_NOT_EXIST;
    }
    accel_find(extension, ext);
    ext->mode_count = 0;
    for (i = 0; i < ENTXP_MODE_COUNT; i++) {
        if ((ULONG)entxp_modes[i].width * entxp_modes[i].height * 4 <= ext->vram_length) {
            ext->mode_count = i + 1;
        }
    }
    ext->current = 0;

    /* No VGA emulation for full-screen DOS boxes, no saved hardware state. */
    config->NumEmulatorAccessEntries = 0;
    config->EmulatorAccessEntries = NULL;
    config->EmulatorAccessEntriesContext = 0;
    config->VdmPhysicalVideoMemoryAddress.QuadPart = 0;
    config->VdmPhysicalVideoMemoryLength = 0;
    config->HardwareStateSize = 0;

    VideoPortSetRegistryParameters(extension, L"HardwareInformation.MemorySize",
                                   &ext->vram_length, sizeof(ULONG));
    return NO_ERROR;
}

BOOLEAN Initialize(PVOID extension)
{
    (void)extension;
    return TRUE;
}

BOOLEAN StartIO(PVOID extension, PVIDEO_REQUEST_PACKET packet)
{
    DEVICE_EXT *ext = (DEVICE_EXT *)extension;
    VP_STATUS status = ERROR_INVALID_FUNCTION;
    ULONG i;

    packet->StatusBlock->Information = 0;
    switch (packet->IoControlCode) {
    case IOCTL_VIDEO_QUERY_NUM_AVAIL_MODES: {
        VIDEO_NUM_MODES *out = (VIDEO_NUM_MODES *)packet->OutputBuffer;
        if (packet->OutputBufferLength < sizeof(*out)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        out->NumModes = ext->mode_count;
        out->ModeInformationLength = sizeof(VIDEO_MODE_INFORMATION);
        packet->StatusBlock->Information = sizeof(*out);
        status = NO_ERROR;
        break;
    }
    case IOCTL_VIDEO_QUERY_AVAIL_MODES: {
        VIDEO_MODE_INFORMATION *out = (VIDEO_MODE_INFORMATION *)packet->OutputBuffer;
        if (packet->OutputBufferLength < ext->mode_count * sizeof(*out)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        for (i = 0; i < ext->mode_count; i++) {
            describe(ext, i, &out[i]);
        }
        packet->StatusBlock->Information = ext->mode_count * sizeof(*out);
        status = NO_ERROR;
        break;
    }
    case IOCTL_VIDEO_QUERY_CURRENT_MODE: {
        VIDEO_MODE_INFORMATION *out = (VIDEO_MODE_INFORMATION *)packet->OutputBuffer;
        if (packet->OutputBufferLength < sizeof(*out)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        describe(ext, ext->current, out);
        packet->StatusBlock->Information = sizeof(*out);
        status = NO_ERROR;
        break;
    }
    case IOCTL_VIDEO_SET_CURRENT_MODE: {
        VIDEO_MODE *in = (VIDEO_MODE *)packet->InputBuffer;
        ULONG index;
        if (packet->InputBufferLength < sizeof(*in)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        index = in->RequestedMode & ~(VIDEO_MODE_NO_ZERO_MEMORY | VIDEO_MODE_MAP_MEM_LINEAR);
        if (index >= ext->mode_count) {
            status = ERROR_INVALID_PARAMETER;
            break;
        }
        set_mode(ext, index, (BOOLEAN)!(in->RequestedMode & VIDEO_MODE_NO_ZERO_MEMORY));
        status = NO_ERROR;
        break;
    }
    case IOCTL_VIDEO_RESET_DEVICE:
        /* Back to the VGA: the framebuffer stops showing. */
        dispi_write(ext, DISPI_ENABLE, 0);
        status = NO_ERROR;
        break;
    case IOCTL_VIDEO_MAP_VIDEO_MEMORY: {
        VIDEO_MEMORY *in = (VIDEO_MEMORY *)packet->InputBuffer;
        VIDEO_MEMORY_INFORMATION *out = (VIDEO_MEMORY_INFORMATION *)packet->OutputBuffer;
        ULONG in_io_space = VIDEO_MEMORY_SPACE_MEMORY;
        ULONG length = ext->vram_length;
        PVOID base;
        if (packet->InputBufferLength < sizeof(*in) ||
            packet->OutputBufferLength < sizeof(*out)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        base = in->RequestedVirtualAddress;
        status = VideoPortMapMemory(extension, ext->vram, &length, &in_io_space, &base);
        if (status != NO_ERROR) {
            break;
        }
        out->VideoRamBase = base;
        out->VideoRamLength = length;
        out->FrameBufferBase = base;
        out->FrameBufferLength = length;
        packet->StatusBlock->Information = sizeof(*out);
        break;
    }
    case IOCTL_ENTXP_ACCEL_RUN: {
        /* Buffered: the input and the output are the same system buffer, so
         * the commands are out of it before the response goes in. */
        ULONG produced;
        if (ext->accel_commands == NULL) {
            status = ERROR_DEV_NOT_EXIST;
            break;
        }
        if (packet->InputBufferLength > ext->accel_commands_bytes) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        VideoPortMoveMemory(ext->accel_commands, packet->InputBuffer, packet->InputBufferLength);
        /* The host runs the buffer inside this write. */
        VideoPortWriteRegisterUlong((PULONG)(ext->regs + ACCEL_BASE + 4 * ACCEL_RUN),
                                    packet->InputBufferLength);
        produced = accel_read(ext, ACCEL_RUN);
        if (produced > ext->accel_response_bytes) {
            produced = ext->accel_response_bytes;
        }
        if (produced > packet->OutputBufferLength) {
            produced = packet->OutputBufferLength;
        }
        VideoPortMoveMemory(packet->OutputBuffer, ext->accel_response, produced);
        packet->StatusBlock->Information = produced;
        status = NO_ERROR;
        break;
    }
    case IOCTL_VIDEO_UNMAP_VIDEO_MEMORY: {
        VIDEO_MEMORY *in = (VIDEO_MEMORY *)packet->InputBuffer;
        if (packet->InputBufferLength < sizeof(*in)) {
            status = ERROR_INSUFFICIENT_BUFFER;
            break;
        }
        status = VideoPortUnmapMemory(extension, in->RequestedVirtualAddress, 0);
        break;
    }
    default:
        break;
    }
    packet->StatusBlock->Status = status;
    return TRUE;
}

ULONG DriverEntry(PVOID context1, PVOID context2)
{
    VIDEO_HW_INITIALIZATION_DATA init;

    VideoPortZeroMemory(&init, sizeof(init));
    init.HwInitDataSize = sizeof(init);
    init.HwFindAdapter = FindAdapter;
    init.HwInitialize = Initialize;
    init.HwStartIO = StartIO;
    init.HwDeviceExtensionSize = sizeof(DEVICE_EXT);
    init.AdapterInterfaceType = PCIBus;
    return VideoPortInitialize(context1, context2, &init, NULL);
}
