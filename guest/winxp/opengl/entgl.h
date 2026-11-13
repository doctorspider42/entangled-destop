/*
 * What the three pieces of the XP OpenGL driver share (ADR-0013):
 *
 *   entgl.dll  the OpenGL driver programs load (an "ICD"), user mode;
 *   entxp.dll  the display driver, in win32k: forwards an escape;
 *   entxp.sys  the video miniport: owns the device's registers.
 *
 * A buffer of OpenGL commands travels  ExtEscape -> DrvEscape -> IOCTL ->
 * the top of VRAM -> one register write, and the host runs it before the
 * write returns. The stream's format is the host's
 * (crates/gl-passthrough/src/lib.rs); nothing below the ICD looks inside.
 */
#ifndef ENTGL_H
#define ENTGL_H

/* ExtEscape number: input is a command stream, output its response.
 * Returns 0 when there is no accelerator, else 1 + response bytes. */
#define ENTGL_ESCAPE 0x454c /* 'EL' */

/* The miniport's IOCTL behind it: CTL_CODE(FILE_DEVICE_VIDEO, 0x900,
 * METHOD_BUFFERED, FILE_ANY_ACCESS). */
#define IOCTL_ENTXP_ACCEL_RUN ((0x23u << 16) | (0x900u << 2))

/* The miniport's log IOCTL, CTL_CODE(FILE_DEVICE_VIDEO, 0x901, ...): the
 * input is text for the host's log. And the register the text goes to, a
 * byte at a time; a line feed ends a line. */
#define IOCTL_ENTXP_LOG ((0x23u << 16) | (0x901u << 2))
#define LOG_REGISTER 0x700

/* The device's accelerator registers: 32 bits each, from this offset of
 * the register page (machine_x86::framebuffer::accel). */
#define ACCEL_BASE 0x600
#define ACCEL_ID 0
#define ACCEL_COMMANDS 1
#define ACCEL_COMMANDS_BYTES 2
#define ACCEL_RESPONSE 3
#define ACCEL_RESPONSE_BYTES 4
#define ACCEL_RUN 5
#define ACCEL_MAGIC 0x4c474e45u /* "ENGL" */

/* The protocol's own opcodes (gl_passthrough::control). */
#define ENTGL_CONTEXT_CREATE 0x8000
#define ENTGL_CONTEXT_DESTROY 0x8001
#define ENTGL_SELECT 0x8002
#define ENTGL_SWAP 0x8003
#define ENTGL_DRAW_ARRAYS 0x8004
#define ENTGL_DRAW_ELEMENTS 0x8005
#define ENTGL_CONTEXT_COPY 0x8006
#define ENTGL_CONTEXT_SHARE 0x8007

/*
 * The pixel formats, numbered from 1: all 32-bit BGRA; with and without a
 * back buffer; a 24-bit depth buffer with 8 bits of stencil, or 16 and
 * none. (The host gives every context 24 and 8 — a program that asked for
 * less does not mind.) The display driver and the ICD must agree: GDI asks
 * one, opengl32.dll the other.
 */
#define ENTGL_FORMAT_COUNT 4
#define ENTGL_FORMAT_DOUBLE(i) (((i) - 1) % 2 == 0)
#define ENTGL_FORMAT_DEPTH(i) ((i) <= 2 ? 24 : 16)
#define ENTGL_FORMAT_STENCIL(i) ((i) <= 2 ? 8 : 0)

/* Fills *pfd for format i (1-based); needs wingdi.h's PIXELFORMATDESCRIPTOR. */
#define ENTGL_DESCRIBE(pfd, i)                                                          \
    do {                                                                                \
        unsigned char *bytes_ = (unsigned char *)(pfd);                                 \
        unsigned n_;                                                                    \
        for (n_ = 0; n_ < sizeof(PIXELFORMATDESCRIPTOR); n_++) {                        \
            bytes_[n_] = 0;                                                             \
        }                                                                               \
        (pfd)->nSize = sizeof(PIXELFORMATDESCRIPTOR);                                   \
        (pfd)->nVersion = 1;                                                            \
        (pfd)->dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL |                      \
                         (ENTGL_FORMAT_DOUBLE(i) ? PFD_DOUBLEBUFFER | PFD_SWAP_COPY : 0); \
        (pfd)->iPixelType = PFD_TYPE_RGBA;                                              \
        (pfd)->cColorBits = 32;                                                         \
        (pfd)->cRedBits = 8;                                                            \
        (pfd)->cRedShift = 16;                                                          \
        (pfd)->cGreenBits = 8;                                                          \
        (pfd)->cGreenShift = 8;                                                         \
        (pfd)->cBlueBits = 8;                                                           \
        (pfd)->cBlueShift = 0;                                                          \
        (pfd)->cAlphaBits = 8;                                                          \
        (pfd)->cAlphaShift = 24;                                                        \
        (pfd)->cDepthBits = ENTGL_FORMAT_DEPTH(i);                                      \
        (pfd)->cStencilBits = ENTGL_FORMAT_STENCIL(i);                                  \
        (pfd)->iLayerType = PFD_MAIN_PLANE;                                             \
    } while (0)

#endif
