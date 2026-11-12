/*
 * What the miniport (entxp.sys) and the display driver (entxp.dll) share:
 * the mode list. A mode's index in this table is its mode number on both
 * sides of the IOCTLs; all modes are 32 bits a pixel.
 */
#ifndef ENTXP_H
#define ENTXP_H

typedef struct {
    unsigned short width;
    unsigned short height;
} ENTXP_MODE;

static const ENTXP_MODE entxp_modes[] = {
    {640, 480},   {800, 600},   {1024, 768},  {1152, 864},  {1280, 720},  {1280, 768},
    {1280, 800},  {1280, 960},  {1280, 1024}, {1366, 768},  {1440, 900},  {1600, 900},
    {1600, 1200}, {1680, 1050}, {1920, 1080}, {1920, 1200},
};

#define ENTXP_MODE_COUNT (sizeof(entxp_modes) / sizeof(entxp_modes[0]))

#endif
