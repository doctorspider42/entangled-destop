/*
 * entxp.dll — the display driver of the Entangled display driver for
 * Windows XP (ADR-0013, phase G0).
 *
 * The smallest display driver GDI accepts: it describes the device, sets
 * the mode through the miniport (entxp.sys), and hands GDI the linear
 * framebuffer as an engine-managed bitmap. Every drawing call is then
 * GDI's own software rendering into VRAM, which the host presents; the
 * driver hooks nothing. The cursor is GDI's too.
 *
 * Runs in kernel mode inside win32k.sys and links against nothing else.
 * Freestanding C, built with clang; see ../build.sh.
 */
#include <stdarg.h>
#include <stddef.h>

#include <windef.h>
#include <wingdi.h>
#include <winddi.h>
#include <devioctl.h>
#include <ntddvdeo.h>

#include "entxp.h"

#define POOL_TAG 0x70787445 /* 'Etxp' */

typedef struct {
    HANDLE driver;   /* the miniport, for EngDeviceIoControl */
    HDEV hdev;
    HSURF surface;
    HPALETTE palette;
    ULONG mode;      /* index into entxp_modes */
    PVOID vram;      /* mapped while the surface exists */
} PDEV;

/* The compiler may ask for these; win32k exports neither under a C name. */
void *__cdecl memset(void *destination, int value, size_t count)
{
    unsigned char *out = (unsigned char *)destination;
    while (count--) {
        *out++ = (unsigned char)value;
    }
    return destination;
}

void *__cdecl memcpy(void *destination, const void *source, size_t count)
{
    unsigned char *out = (unsigned char *)destination;
    const unsigned char *in = (const unsigned char *)source;
    while (count--) {
        *out++ = *in++;
    }
    return destination;
}

static BOOL ioctl(HANDLE driver, DWORD code, PVOID in, ULONG in_size, PVOID out, ULONG out_size)
{
    ULONG returned = 0;
    return EngDeviceIoControl(driver, code, in, in_size, out, out_size, &returned) == 0;
}

/* How many of the table's modes the miniport offers (those that fit VRAM). */
static ULONG mode_count(HANDLE driver)
{
    VIDEO_NUM_MODES modes;
    if (!ioctl(driver, IOCTL_VIDEO_QUERY_NUM_AVAIL_MODES, NULL, 0, &modes, sizeof(modes))) {
        return 0;
    }
    return modes.NumModes < ENTXP_MODE_COUNT ? modes.NumModes : ENTXP_MODE_COUNT;
}

static void copy_wide(WCHAR *to, const WCHAR *from, ULONG capacity)
{
    ULONG i = 0;
    while (i + 1 < capacity && from[i]) {
        to[i] = from[i];
        i++;
    }
    to[i] = 0;
}

static void default_font(LOGFONTW *font, LONG height, LONG width, LONG weight, BYTE pitch,
                         BYTE clip, BYTE quality, const WCHAR *face)
{
    memset(font, 0, sizeof(*font));
    font->lfHeight = height;
    font->lfWidth = width;
    font->lfWeight = weight;
    font->lfCharSet = ANSI_CHARSET;
    font->lfOutPrecision = OUT_DEFAULT_PRECIS;
    font->lfClipPrecision = clip;
    font->lfQuality = quality;
    font->lfPitchAndFamily = pitch;
    copy_wide(font->lfFaceName, face, LF_FACESIZE);
}

DHPDEV APIENTRY DrvEnablePDEV(DEVMODEW *devmode, LPWSTR logical_address, ULONG pattern_count,
                              HSURF *patterns, ULONG caps_size, ULONG *caps, ULONG info_size,
                              DEVINFO *info, HDEV hdev, LPWSTR device_name, HANDLE driver)
{
    PDEV *pdev;
    GDIINFO gdi;
    DEVINFO dev;
    ULONG count = mode_count(driver);
    ULONG index = 0;
    ULONG i;

    (void)logical_address;
    (void)pattern_count;
    (void)patterns;
    (void)device_name;
    if (count == 0) {
        return NULL;
    }
    /* The mode asked for; with nothing asked (a first start), 1024x768 if
     * the device has it, else its first. */
    for (i = 0; i < count; i++) {
        if (entxp_modes[i].width == 1024 && entxp_modes[i].height == 768) {
            index = i;
        }
    }
    if (devmode != NULL && devmode->dmPelsWidth != 0 && devmode->dmPelsHeight != 0) {
        BOOL found = FALSE;
        if (devmode->dmBitsPerPel != 0 && devmode->dmBitsPerPel != 32) {
            return NULL;
        }
        for (i = 0; i < count; i++) {
            if (entxp_modes[i].width == devmode->dmPelsWidth &&
                entxp_modes[i].height == devmode->dmPelsHeight) {
                index = i;
                found = TRUE;
            }
        }
        if (!found) {
            return NULL;
        }
    }

    pdev = (PDEV *)EngAllocMem(FL_ZERO_MEMORY, sizeof(PDEV), POOL_TAG);
    if (pdev == NULL) {
        return NULL;
    }
    pdev->driver = driver;
    pdev->hdev = hdev;
    pdev->mode = index;
    pdev->palette = EngCreatePalette(PAL_BITFIELDS, 0, NULL, 0x00ff0000, 0x0000ff00, 0x000000ff);
    if (pdev->palette == NULL) {
        EngFreeMem(pdev);
        return NULL;
    }

    memset(&gdi, 0, sizeof(gdi));
    gdi.ulVersion = 0x5000;
    gdi.ulTechnology = DT_RASDISPLAY;
    gdi.ulHorzSize = 320;
    gdi.ulVertSize = 240;
    gdi.ulHorzRes = entxp_modes[index].width;
    gdi.ulVertRes = entxp_modes[index].height;
    gdi.cBitsPixel = 32;
    gdi.cPlanes = 1;
    gdi.ulNumColors = (ULONG)-1;
    gdi.ulLogPixelsX = 96;
    gdi.ulLogPixelsY = 96;
    gdi.flTextCaps = TC_RA_ABLE;
    gdi.ulDACRed = 8;
    gdi.ulDACGreen = 8;
    gdi.ulDACBlue = 8;
    gdi.ulAspectX = 0x24;
    gdi.ulAspectY = 0x24;
    gdi.ulAspectXY = 0x33;
    gdi.xStyleStep = 1;
    gdi.yStyleStep = 1;
    gdi.denStyleStep = 3;
    /* The colorimetry every sample display driver reports: an sRGB-like
     * monitor with a gamma of 2.0. */
    gdi.ciDevice.Red.x = 6700;
    gdi.ciDevice.Red.y = 3300;
    gdi.ciDevice.Green.x = 2100;
    gdi.ciDevice.Green.y = 7100;
    gdi.ciDevice.Blue.x = 1400;
    gdi.ciDevice.Blue.y = 800;
    gdi.ciDevice.AlignmentWhite.x = 3127;
    gdi.ciDevice.AlignmentWhite.y = 3290;
    gdi.ciDevice.RedGamma = 20000;
    gdi.ciDevice.GreenGamma = 20000;
    gdi.ciDevice.BlueGamma = 20000;
    gdi.ulPrimaryOrder = PRIMARY_ORDER_CBA;
    gdi.ulHTPatternSize = HT_PATSIZE_4x4_M;
    gdi.ulHTOutputFormat = HT_FORMAT_32BPP;
    gdi.flHTFlags = HT_FLAG_ADDITIVE_PRIMS;
    gdi.ulVRefresh = 60;
    memcpy(caps, &gdi, caps_size < sizeof(gdi) ? caps_size : sizeof(gdi));

    memset(&dev, 0, sizeof(dev));
    default_font(&dev.lfDefaultFont, 16, 7, 700, VARIABLE_PITCH | FF_DONTCARE,
                 CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY, L"System");
    default_font(&dev.lfAnsiVarFont, 12, 9, 400, VARIABLE_PITCH | FF_DONTCARE,
                 CLIP_STROKE_PRECIS, PROOF_QUALITY, L"MS Sans Serif");
    default_font(&dev.lfAnsiFixFont, 12, 9, 400, FIXED_PITCH | FF_DONTCARE,
                 CLIP_STROKE_PRECIS, PROOF_QUALITY, L"Courier");
    dev.iDitherFormat = BMF_32BPP;
    dev.cxDither = 8;
    dev.cyDither = 8;
    dev.hpalDefault = pdev->palette;
    memcpy(info, &dev, info_size < sizeof(dev) ? info_size : sizeof(dev));

    return (DHPDEV)pdev;
}

VOID APIENTRY DrvCompletePDEV(DHPDEV dhpdev, HDEV hdev)
{
    ((PDEV *)dhpdev)->hdev = hdev;
}

VOID APIENTRY DrvDisablePDEV(DHPDEV dhpdev)
{
    PDEV *pdev = (PDEV *)dhpdev;
    if (pdev->palette != NULL) {
        EngDeletePalette(pdev->palette);
    }
    EngFreeMem(pdev);
}

static BOOL show_mode(PDEV *pdev)
{
    VIDEO_MODE mode;
    mode.RequestedMode = pdev->mode;
    return ioctl(pdev->driver, IOCTL_VIDEO_SET_CURRENT_MODE, &mode, sizeof(mode), NULL, 0);
}

HSURF APIENTRY DrvEnableSurface(DHPDEV dhpdev)
{
    PDEV *pdev = (PDEV *)dhpdev;
    VIDEO_MEMORY wanted;
    VIDEO_MEMORY_INFORMATION mapped;
    SIZEL size;
    HSURF surface;

    if (!show_mode(pdev)) {
        return NULL;
    }
    wanted.RequestedVirtualAddress = NULL;
    if (!ioctl(pdev->driver, IOCTL_VIDEO_MAP_VIDEO_MEMORY, &wanted, sizeof(wanted), &mapped,
               sizeof(mapped))) {
        return NULL;
    }
    pdev->vram = mapped.FrameBufferBase;
    size.cx = entxp_modes[pdev->mode].width;
    size.cy = entxp_modes[pdev->mode].height;
    /* The framebuffer itself as GDI's bitmap: top-down, 32 bits a pixel. */
    surface = (HSURF)EngCreateBitmap(size, size.cx * 4, BMF_32BPP, BMF_TOPDOWN, pdev->vram);
    if (surface == NULL || !EngAssociateSurface(surface, pdev->hdev, 0)) {
        if (surface != NULL) {
            EngDeleteSurface(surface);
        }
        wanted.RequestedVirtualAddress = pdev->vram;
        ioctl(pdev->driver, IOCTL_VIDEO_UNMAP_VIDEO_MEMORY, &wanted, sizeof(wanted), NULL, 0);
        pdev->vram = NULL;
        return NULL;
    }
    pdev->surface = surface;
    return surface;
}

VOID APIENTRY DrvDisableSurface(DHPDEV dhpdev)
{
    PDEV *pdev = (PDEV *)dhpdev;
    VIDEO_MEMORY mapped;

    if (pdev->surface != NULL) {
        EngDeleteSurface(pdev->surface);
        pdev->surface = NULL;
    }
    if (pdev->vram != NULL) {
        mapped.RequestedVirtualAddress = pdev->vram;
        ioctl(pdev->driver, IOCTL_VIDEO_UNMAP_VIDEO_MEMORY, &mapped, sizeof(mapped), NULL, 0);
        pdev->vram = NULL;
    }
}

BOOL APIENTRY DrvAssertMode(DHPDEV dhpdev, BOOL enable)
{
    PDEV *pdev = (PDEV *)dhpdev;
    if (enable) {
        return show_mode(pdev);
    }
    /* A full-screen text session takes the screen: back to the VGA. */
    return ioctl(pdev->driver, IOCTL_VIDEO_RESET_DEVICE, NULL, 0, NULL, 0);
}

ULONG APIENTRY DrvGetModes(HANDLE driver, ULONG size, DEVMODEW *devmode)
{
    ULONG count = mode_count(driver);
    ULONG needed = count * sizeof(DEVMODEW);
    ULONG i;

    if (devmode == NULL) {
        return needed;
    }
    if (size < needed) {
        return 0;
    }
    for (i = 0; i < count; i++) {
        DEVMODEW *out = &devmode[i];
        memset(out, 0, sizeof(*out));
        copy_wide(out->dmDeviceName, L"entxp", CCHDEVICENAME);
        out->dmSpecVersion = DM_SPECVERSION;
        out->dmDriverVersion = DM_SPECVERSION;
        out->dmSize = sizeof(DEVMODEW);
        out->dmBitsPerPel = 32;
        out->dmPelsWidth = entxp_modes[i].width;
        out->dmPelsHeight = entxp_modes[i].height;
        out->dmDisplayFrequency = 60;
        out->dmFields = DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY |
                        DM_DISPLAYFLAGS;
    }
    return needed;
}

static DRVFN functions[] = {
    {INDEX_DrvEnablePDEV, (PFN)DrvEnablePDEV},
    {INDEX_DrvCompletePDEV, (PFN)DrvCompletePDEV},
    {INDEX_DrvDisablePDEV, (PFN)DrvDisablePDEV},
    {INDEX_DrvEnableSurface, (PFN)DrvEnableSurface},
    {INDEX_DrvDisableSurface, (PFN)DrvDisableSurface},
    {INDEX_DrvAssertMode, (PFN)DrvAssertMode},
    {INDEX_DrvGetModes, (PFN)DrvGetModes},
};

BOOL APIENTRY DrvEnableDriver(ULONG engine_version, ULONG size, DRVENABLEDATA *data)
{
    (void)engine_version;
    if (size < sizeof(DRVENABLEDATA)) {
        return FALSE;
    }
    data->iDriverVersion = DDI_DRIVER_VERSION_NT5;
    data->c = sizeof(functions) / sizeof(functions[0]);
    data->pdrvfn = functions;
    return TRUE;
}
