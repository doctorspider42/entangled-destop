// SPDX-License-Identifier: Apache-2.0
// Guest-side timing probe for the animation lab shortcuts.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <charconv>
#include <cstdint>
#include <cstdio>
#include <cstring>

namespace {
HWND target;
bool windows_key;
bool shift;
bool control;
bool alt;

void record(const char *event, DWORD message_ms)
{
    LARGE_INTEGER qpc{};
    QueryPerformanceCounter(&qpc);
    printf("%s,%lld,%lu\n", event, qpc.QuadPart, static_cast<unsigned long>(message_ms));
}

void CALLBACK window_event(HWINEVENTHOOK, DWORD event, HWND hwnd, LONG object, LONG child,
                           DWORD, DWORD message_ms)
{
    if (hwnd == target && object == OBJID_WINDOW && child == CHILDID_SELF) {
        record(event == EVENT_SYSTEM_MINIMIZESTART ? "window_minimize" : "window_restore", message_ms);
    }
}

LRESULT CALLBACK keyboard_event(int code, WPARAM message, LPARAM data)
{
    if (code != HC_ACTION)
        return CallNextHookEx(nullptr, code, message, data);
    const auto &input = *reinterpret_cast<const KBDLLHOOKSTRUCT *>(data);
    const auto key = input.vkCode;
    const bool down = (input.flags & LLKHF_UP) == 0;
    switch (key) {
    case VK_LWIN:
    case VK_RWIN:
        windows_key = down;
        break;
    case VK_SHIFT:
    case VK_LSHIFT:
    case VK_RSHIFT:
        shift = down;
        break;
    case VK_CONTROL:
    case VK_LCONTROL:
    case VK_RCONTROL:
        control = down;
        break;
    case VK_MENU:
    case VK_LMENU:
    case VK_RMENU:
        alt = down;
        break;
    default:
        break;
    }
    // Only emit the specific lab shortcuts. Pass all input through unchanged.
    if (down && windows_key && key == 'M') {
        record(shift ? "key_restore" : "key_minimize", input.time);
    } else if (down && control && alt && (key == VK_LEFT || key == VK_RIGHT)) {
        record(key == VK_LEFT ? "key_cube_left" : "key_cube_right", input.time);
    }
    return CallNextHookEx(nullptr, code, message, data);
}

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam)
{
    if (message == WM_TIMER) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(window, message, wparam, lparam);
}

template <typename T>
bool parse(const char *start, T &out, int base)
{
    if (base == 16 && std::strncmp(start, "0x", 2) == 0)
        start += 2;
    const char *end = start + std::strlen(start);
    const auto value = std::from_chars(start, end, out, base);
    return value.ec == std::errc{} && value.ptr == end;
}

int win32_error(const char *api)
{
    fprintf(stderr, "%s failed (%lu)\n", api, static_cast<unsigned long>(GetLastError()));
    return 1;
}
} // namespace

int main(int argc, char **argv)
{
    uintptr_t value = 0;
    UINT seconds = 0;
    if (argc != 3 || !parse(argv[1], value, 16) || !parse(argv[2], seconds, 10) ||
        seconds == 0 || seconds > 180 || !IsWindow(reinterpret_cast<HWND>(value))) {
        fprintf(stderr, "usage: animation-input-qpc.exe EXPLORER_HWND_HEX SECONDS_1_TO_180\n");
        return 2;
    }
    target = reinterpret_cast<HWND>(value);
    DWORD process = 0;
    if (!GetWindowThreadProcessId(target, &process))
        return win32_error("GetWindowThreadProcessId");

    WNDCLASSW cls{};
    cls.hInstance = GetModuleHandleW(nullptr);
    cls.lpszClassName = L"EntangledAnimationInputProbe";
    cls.lpfnWndProc = window_proc;
    if (!RegisterClassW(&cls))
        return win32_error("RegisterClass");
    HWND window = CreateWindowExW(0, cls.lpszClassName, L"", 0, 0, 0, 0, 0,
                                  HWND_MESSAGE, nullptr, cls.hInstance, nullptr);
    if (!window)
        return win32_error("CreateWindowEx");
    if (!SetTimer(window, 1, seconds * 1000, nullptr))
        return win32_error("SetTimer");
    const auto hook = SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND,
                                      nullptr, window_event, process, 0, WINEVENT_OUTOFCONTEXT);
    if (!hook)
        return win32_error("SetWinEventHook");
    const auto keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, keyboard_event, cls.hInstance, 0);
    if (!keyboard) {
        const auto status = win32_error("SetWindowsHookEx");
        UnhookWinEvent(hook);
        return status;
    }

    LARGE_INTEGER frequency{};
    QueryPerformanceFrequency(&frequency);
    fprintf(stderr, "ready qpc_frequency=%lld\n", frequency.QuadPart);
    fflush(stderr);
    puts("event,qpc,message_ms");
    fflush(stdout);
    MSG message{};
    BOOL result;
    while ((result = GetMessageW(&message, nullptr, 0, 0)) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    const auto status = result < 0 ? win32_error("GetMessage") : 0;
    UnhookWindowsHookEx(keyboard);
    UnhookWinEvent(hook);
    return status;
}
