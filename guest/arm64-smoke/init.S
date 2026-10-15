// Minimal Linux ARM64 PID 1: write the shared boot marker and power off.
// No libc or host-specific code is part of this guest program.
.text
.global _start
_start:
    mov x0, #1
    adr x1, .Lmessage
    mov x2, #(.Lend - .Lmessage)
    mov x8, #64
    svc #0
    movz x0, #0xdead
    movk x0, #0xfee1, lsl #16
    movz x1, #0x1969
    movk x1, #0x2812, lsl #16
    movz x2, #0xfedc
    movk x2, #0x4321, lsl #16
    mov x8, #142
    svc #0
.Lwait:
    b .Lwait
.Lmessage:
    .ascii "VMHOST_GUEST_READY ARM64\n"
.Lend:
