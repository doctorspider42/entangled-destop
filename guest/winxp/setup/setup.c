/*
 * entxp-setup.exe — installs the Entangled drivers on Windows XP
 * (ADR-0013).
 *
 * XP has no command that installs a Plug and Play driver from an INF; the
 * API for it is one call into newdev.dll. This program makes that call for
 * each INF beside it, so the unattended install (or a person) can run one
 * file from the additions disc.
 *
 * No C runtime: kernel32 and newdev only. Built with clang; ../build.sh.
 */
#include <windows.h>

#define INSTALLFLAG_FORCE 0x00000001

BOOL WINAPI UpdateDriverForPlugAndPlayDevicesW(HWND parent, LPCWSTR hardware_id,
                                               LPCWSTR inf_path, DWORD flags, PBOOL reboot);

typedef struct {
    const WCHAR *inf;
    const WCHAR *hardware_id;
} DRIVER;

static const DRIVER drivers[] = {
    {L"entxp.inf", L"PCI\\VEN_1234&DEV_1111"},
};

static void say(const char *text)
{
    DWORD written;
    DWORD length = 0;
    while (text[length]) {
        length++;
    }
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), text, length, &written, NULL);
}

void entry(void)
{
    WCHAR path[MAX_PATH + 32];
    DWORD length = GetModuleFileNameW(NULL, path, MAX_PATH);
    DWORD failures = 0;
    unsigned i, j;

    /* The directory this program is in, with its trailing backslash. */
    while (length > 0 && path[length - 1] != L'\\') {
        length--;
    }
    for (i = 0; i < sizeof(drivers) / sizeof(drivers[0]); i++) {
        BOOL reboot = FALSE;
        for (j = 0; drivers[i].inf[j] && length + j + 1 < MAX_PATH + 32; j++) {
            path[length + j] = drivers[i].inf[j];
        }
        path[length + j] = 0;
        if (UpdateDriverForPlugAndPlayDevicesW(NULL, drivers[i].hardware_id, path,
                                               INSTALLFLAG_FORCE, &reboot)) {
            say("installed a driver\r\n");
        } else {
            say("a driver was not installed\r\n");
            failures++;
        }
    }
    ExitProcess(failures);
}
