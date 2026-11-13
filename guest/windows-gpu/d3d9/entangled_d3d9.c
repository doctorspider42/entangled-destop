/*
 * entangled_d3d9.dll: the Direct3D 9 user-mode driver of entangled-wddm.sys
 * (ADR-0011). Windows' d3d9.dll loads it for the display adapter and calls
 * OpenAdapter; everything else goes through the two tables it fills in.
 *
 * Phase N0: the runtime takes the driver. The adapter answers its caps and
 * makes a device whose every function answers S_OK and says its name once -
 * which is the list of what a program calls, in the order it calls it.
 * Nothing is drawn yet.
 */
#include "ddi.h"
#include "d3d9_nine.h"

/* The kernel driver's contract for an allocation's private data. */
#include "wddm/viogpu_wddm_driver.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ log */

/* The kernel driver prints an escape of this type on the VM's serial log
 * (a diagnostic build of it; a quiet one drops it). */
#define ESCAPE_PRINT 0x900

/* The log goes through gdi32's thunks (d3dkmthk.h) and an adapter handle of
 * its own, so that the adapter-level code can speak before there is a device. */
static D3DKMT_HANDLE log_adapter;
static CRITICAL_SECTION log_lock;
static BOOL log_ready;

static void
d3d9_log(const char *format, ...)
{
   struct {
      unsigned short type;
      unsigned short length;
      char text[236];
   } escape;
   D3DKMT_ESCAPE call;
   va_list args;
   int at;

   char line[236];
   NTSTATUS opened = 0, escaped = 0;

   if (!log_ready)
      return;
   va_start(args, format);
   _vsnprintf(line, sizeof line - 1, format, args);
   va_end(args);
   line[sizeof line - 1] = 0;
   EnterCriticalSection(&log_lock);
   if (!log_adapter) {
      D3DKMT_OPENADAPTERFROMHDC open;

      memset(&open, 0, sizeof open);
      open.hDc = GetDC(NULL);
      opened = D3DKMTOpenAdapterFromHdc(&open);
      ReleaseDC(NULL, open.hDc);
      if (opened >= 0)
         log_adapter = open.hAdapter;
   }
   if (log_adapter) {
      memset(&escape, 0, sizeof escape);
      escape.type = ESCAPE_PRINT;
      at = _snprintf(escape.text, sizeof escape.text - 1, "d3d9 %lu: %s",
                     (unsigned long)GetCurrentProcessId(), line);
      (void)at;
      escape.length = (unsigned short)strlen(escape.text);

      memset(&call, 0, sizeof call);
      call.hAdapter = log_adapter;
      call.pPrivateDriverData = &escape;
      call.PrivateDriverDataSize = sizeof escape;
      escaped = D3DKMTEscape(&call);
   }
   /* ENTANGLED_D3D9_LOG names a file to say it all in as well: the serial
    * log is the kernel driver's to give, and a quiet build of it gives none. */
   {
      char path[MAX_PATH];

      if (GetEnvironmentVariableA("ENTANGLED_D3D9_LOG", path, sizeof path) - 1 < sizeof path - 1) {
         FILE *file = fopen(path, "a");

         if (file) {
            fprintf(file, "%lu: %s (open 0x%lx, escape 0x%lx)\n",
                    (unsigned long)GetCurrentProcessId(), line, (unsigned long)opened,
                    (unsigned long)escaped);
            fclose(file);
         }
      }
   }
   LeaveCriticalSection(&log_lock);
}

/* ----------------------------------------------------------------- rings */

/* The last device functions called, by name: always kept (a store a call),
 * and printed by the crash report. With ENTANGLED_D3D9_RING=1 a second ring
 * keeps what the ones that matter were told and answered. */
static const char *volatile call_ring[64];
static volatile LONG call_at;
/* ENTANGLED_D3D9_CALLS=1: the first thousand device functions called, by
 * name, placeholders included - what the runtime does and in what order. */
static volatile LONG calls_left = -1;

static void
d3d9_log(const char *format, ...);

static void
d3d9_called(const char *name)
{
   if (calls_left == -1) {
      char want[4] = "";

      GetEnvironmentVariableA("ENTANGLED_D3D9_CALLS", want, sizeof want);
      InterlockedExchange(&calls_left, want[0] == '1' ? 1000 : 0);
   }
   if (calls_left > 0 && InterlockedDecrement(&calls_left) >= 0)
      d3d9_log("call: %s", name);
}
static char note_ring[96][150];
static volatile LONG note_at;
static volatile LONG note_on = -1;

static void
d3d9_note(const char *format, ...)
{
   va_list args;
   char *line;

   if (note_on == -1) {
      char want[4] = "";

      GetEnvironmentVariableA("ENTANGLED_D3D9_RING", want, sizeof want);
      InterlockedExchange(&note_on, want[0] == '1');
   }
   if (!note_on)
      return;
   line = note_ring[(unsigned)InterlockedIncrement(&note_at) % 96];
   va_start(args, format);
   _vsnprintf(line, sizeof note_ring[0] - 1, format, args);
   va_end(args);
   line[sizeof note_ring[0] - 1] = 0;
}

static void
d3d9_print_rings(void)
{
   char line[230];
   int at = 0;
   LONG last = call_at;

   for (int i = 39; i >= 0; i--) {
      const char *name = call_ring[(unsigned)(last - i) & 63];

      if (!name)
         continue;
      /* Without the "d3d9_" every one of them begins with. */
      at += _snprintf(line + at, sizeof line - 1 - at, " %s", name + 5);
      if (at > 150) {
         line[at] = 0;
         d3d9_log("  calls:%s", line);
         at = 0;
      }
   }
   if (at) {
      line[at] = 0;
      d3d9_log("  calls:%s", line);
   }
   if (note_on == 1) {
      last = note_at;
      for (int i = 59; i >= 0; i--) {
         const char *note = note_ring[(unsigned)(last - i) % 96];

         if (last - i > 0 && note[0])
            d3d9_log("  note: %s", note);
      }
   }
}

/* -------------------------------------------------- the runtime's errors */

#if !defined(_WIN64)
/* What d3d9.dll last said to itself. Its retail build keeps the sentences its
 * debug build would print in a table of its own, newest first; where the
 * table is, is known for the builds this was read from, and anything that
 * does not look like the table is left alone. */
static void
d3d9_print_runtime_errors(void)
{
   const BYTE *base = (const BYTE *)GetModuleHandleA("d3d9.dll");
   BYTE table[8 * 0x80];
   SIZE_T got = 0;

   if (!base ||
       !ReadProcessMemory(GetCurrentProcess(), base + 0x177360, table, sizeof table, &got) ||
       got != sizeof table)
      return;
   for (unsigned i = 0; i < 8; i++) {
      const BYTE *entry = table + i * 0x80;
      UINT code, line;
      char text[0x59], file[0x1d];
      BOOL plain = TRUE;

      memcpy(&code, entry, 4);
      memcpy(&line, entry + 0x60, 4);
      memcpy(text, entry + 4, 0x58);
      text[0x58] = 0;
      memcpy(file, entry + 0x64, 0x1c);
      file[0x1c] = 0;
      if (!text[0])
         break;
      for (const char *c = text; *c; c++) {
         if ((unsigned char)*c < 0x20 || (unsigned char)*c > 0x7e)
            plain = FALSE;
      }
      if (!plain)
         break;
      d3d9_log("  runtime: 0x%08x \"%s\" (%s:%u)", code, text, file, line);
   }
}
#endif

/* ---------------------------------------------------------------- traps */

#if !defined(_WIN64)
struct d3d9_trap {
   const char *name;
   UINT rva;           /* where the build this was read from has it */
   UINT length;
   BYTE bytes[12];
   BYTE *at;           /* where this process has it, once armed */
   BYTE saved;
};

static struct d3d9_trap d3d9_traps[] = {
   {"swap chain: dummy or driver primary?", 0xd19e0, 8, {0x39, 0xbb, 0x48, 0x01, 0x00, 0x00, 0x0f, 0x84}},
   {"swap chain: dummy primary", 0xd1a15, 12, {0x8d, 0x85, 0x88, 0xfa, 0xff, 0xff, 0xc7, 0x85, 0x90, 0xfa, 0xff, 0xff}},
   {"swap chain: driver primary", 0xd1beb, 7, {0x8b, 0x8d, 0xfc, 0xfa, 0xff, 0xff, 0xe8}},
   {"swap chain: fallback to one surface", 0xd1e82, 12, {0x89, 0x8d, 0x90, 0xfa, 0xff, 0xff, 0x8d, 0x8d, 0x88, 0xfa, 0xff, 0xff}},
   {"swap chain: after the fallback (eax)", 0xd1ea5, 12, {0x8b, 0xf0, 0x89, 0xb5, 0x04, 0xfb, 0xff, 0xff, 0x89, 0xb5, 0x00, 0xfb}},
   {"swap chain: creates the chain", 0xd1d64, 12, {0x57, 0xff, 0xb5, 0xfc, 0xfa, 0xff, 0xff, 0x8d, 0x85, 0xac, 0xfa, 0xff}},
   {"swap chain: back buffers", 0xd2298, 8, {0x39, 0xbb, 0x40, 0x03, 0x00, 0x00, 0x0f, 0x84}},
   {"CreateSurfaceLH: before the driver", 0x5d298, 12, {0xf6, 0x87, 0x84, 0x00, 0x00, 0x00, 0x02, 0x8b, 0x8d, 0x84, 0xfb, 0xff}},
   {"CreateSurfaceLH: calls the driver", 0x5d2f1, 12, {0xff, 0xd6, 0x8b, 0xf0, 0x89, 0x85, 0x8c, 0xfb, 0xff, 0xff, 0x85, 0xf6}},
   {"CreateSurfaceLH: a primary, matching a mode", 0x5d632, 10, {0xc7, 0x85, 0x24, 0xfb, 0xff, 0xff, 0x03, 0x00, 0x00, 0x00}},
   {"CreateSurfaceLH: no mode matches", 0x9d0db, 10, {0x81, 0xa5, 0x60, 0xfb, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xf7}},
};

static UINT
d3d9_peek(ULONG_PTR address)
{
   UINT value = 0xdeadc0de;
   SIZE_T got = 0;

   if (address < 0x10000 ||
       !ReadProcessMemory(GetCurrentProcess(), (const void *)address, &value, sizeof value, &got))
      return 0xdeadc0de;
   return value;
}

static void
d3d9_arm_traps(void)
{
   static volatile LONG armed;
   char want[4] = "";
   BYTE *base = (BYTE *)GetModuleHandleA("d3d9.dll");
   const IMAGE_NT_HEADERS *nt;
   BYTE *text, *end;

   GetEnvironmentVariableA("ENTANGLED_D3D9_BREAK", want, sizeof want);
   if (want[0] != '1' || !base || InterlockedExchange(&armed, 1))
      return;
   nt = (const IMAGE_NT_HEADERS *)(base + ((const IMAGE_DOS_HEADER *)base)->e_lfanew);
   text = base + nt->OptionalHeader.BaseOfCode;
   end = text + nt->OptionalHeader.SizeOfCode - 16;
   for (unsigned i = 0; i < ARRAYSIZE(d3d9_traps); i++) {
      struct d3d9_trap *trap = &d3d9_traps[i];
      BYTE *found = NULL;
      unsigned matches = 0;
      DWORD old;

      /* The other build has it a few hundred bytes lower at most. */
      for (BYTE *at = base + trap->rva - 0x500; at <= base + trap->rva + 0x40; at++) {
         if (at < text || at >= end || memcmp(at, trap->bytes, trap->length) != 0)
            continue;
         found = at;
         matches++;
      }
      if (matches != 1) {
         d3d9_log("trap \"%s\": %u places, not armed", trap->name, matches);
         continue;
      }
      if (!VirtualProtect(found, 1, PAGE_EXECUTE_READWRITE, &old))
         continue;
      trap->saved = *found;
      trap->at = found;
      *found = 0xcc;
      VirtualProtect(found, 1, old, &old);
      FlushInstructionCache(GetCurrentProcess(), found, 1);
      d3d9_log("trap \"%s\" at d3d9+0x%Ix", trap->name, (size_t)(found - base));
   }
}

/* A breakpoint of ours: say what is there, put the byte back, go on. */
static BOOL
d3d9_trap_hit(EXCEPTION_POINTERS *info)
{
   BYTE *where = info->ExceptionRecord->ExceptionAddress;

   for (unsigned i = 0; i < ARRAYSIZE(d3d9_traps); i++) {
      struct d3d9_trap *trap = &d3d9_traps[i];
      CONTEXT *c = info->ContextRecord;
      DWORD old;

      if (trap->at != where)
         continue;
      if (VirtualProtect(where, 1, PAGE_EXECUTE_READWRITE, &old)) {
         *where = trap->saved;
         VirtualProtect(where, 1, old, &old);
         FlushInstructionCache(GetCurrentProcess(), where, 1);
      }
      trap->at = NULL;
      c->Eip = (DWORD)(ULONG_PTR)where;
      d3d9_log("trap \"%s\": eax %lx ebx %lx ecx %lx edx %lx esi %lx edi %lx", trap->name,
               c->Eax, c->Ebx, c->Ecx, c->Edx, c->Esi, c->Edi);
      d3d9_log("  [ebx+8] %x [ebx+0x14] %x [ebx+0x10c] %x [ebx+0x114] %x [ebx+0x148] %x "
               "[ebx+0x340] %x [edi+0x84] %x [ebp-0x4a0] %x",
               d3d9_peek(c->Ebx + 8), d3d9_peek(c->Ebx + 0x14), d3d9_peek(c->Ebx + 0x10c),
               d3d9_peek(c->Ebx + 0x114), d3d9_peek(c->Ebx + 0x148), d3d9_peek(c->Ebx + 0x340),
               d3d9_peek(c->Edi + 0x84), d3d9_peek(c->Ebp - 0x4a0));
      d3d9_log("  [ebp-0x514] %x [ebp-0x508] %x [ebp-0x510] %x [ebp-0x504] %x [ebp-0x570] %x",
               d3d9_peek(c->Ebp - 0x514), d3d9_peek(c->Ebp - 0x508), d3d9_peek(c->Ebp - 0x510),
               d3d9_peek(c->Ebp - 0x504), d3d9_peek(c->Ebp - 0x570));
      return TRUE;
   }
   return FALSE;
}
#endif

/* ---------------------------------------------------------------- crash */

/* A fault in the process this DLL is loaded into: say where, once, and let
 * it take its course. A program that dies inside its Direct3D driver says
 * nothing itself, and there is no debugger in the guest. The report is the
 * faulting address by module, the callers unwound (x64), and every word of
 * the stack above it that points into this DLL, as offsets its linker map
 * (/MAP) turns into function names: crude, the same for x64 and x86. */
static HMODULE crash_module;
static PVOID crash_handler;

/* "module+0xoffset" for an address in the process. */
static void
d3d9_where(const void *address, char *text, size_t size)
{
   HMODULE module = NULL;
   char path[MAX_PATH];
   const char *name = "?";

   if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                             GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                          (LPCSTR)address, &module) &&
       module && GetModuleFileNameA(module, path, sizeof path)) {
      const char *slash = strrchr(path, 92);

      name = slash ? slash + 1 : path;
   }
   _snprintf(text, size - 1, "%s+0x%Ix", name, (size_t)((ULONG_PTR)address - (ULONG_PTR)module));
   text[size - 1] = 0;
   /* The nearest export below it, when the module has any: the system DLLs'
    * names are most of what a stack through them says. */
   if (module) {
      const BYTE *base = (const BYTE *)module;
      const IMAGE_NT_HEADERS *nt =
         (const IMAGE_NT_HEADERS *)(base + ((const IMAGE_DOS_HEADER *)base)->e_lfanew);
      const IMAGE_DATA_DIRECTORY *entry =
         &nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
      DWORD rva = (DWORD)((ULONG_PTR)address - (ULONG_PTR)module), best = 0;
      const char *best_name = NULL;

      if (entry->VirtualAddress && entry->Size) {
         const IMAGE_EXPORT_DIRECTORY *exports =
            (const IMAGE_EXPORT_DIRECTORY *)(base + entry->VirtualAddress);
         const DWORD *functions = (const DWORD *)(base + exports->AddressOfFunctions);
         const DWORD *names = (const DWORD *)(base + exports->AddressOfNames);
         const WORD *ordinals = (const WORD *)(base + exports->AddressOfNameOrdinals);

         for (DWORD i = 0; i < exports->NumberOfNames; i++) {
            DWORD at = functions[ordinals[i]];

            if (at <= rva && at > best) {
               best = at;
               best_name = (const char *)(base + names[i]);
            }
         }
      }
      if (best_name && rva - best < 0x2000) {
         size_t used = strlen(text);

         _snprintf(text + used, size - 1 - used, "(%s+0x%lx)", best_name,
                   (unsigned long)(rva - best));
         text[size - 1] = 0;
      }
   }
}

static LONG CALLBACK
d3d9_crash(EXCEPTION_POINTERS *info)
{
   static volatile LONG reported;
   DWORD code = info->ExceptionRecord->ExceptionCode;
   ULONG_PTR base = (ULONG_PTR)crash_module, top, *word, *limit;
   IMAGE_NT_HEADERS *nt;
   char line[260];
   int at = 0;
   unsigned found = 0;

   /* A debug string (Mesa's log goes out through OutputDebugString, which
    * raises this when no debugger listens): carry the text to the log. */
   if (code == 0x40010006u && info->ExceptionRecord->NumberParameters >= 2 &&
       info->ExceptionRecord->ExceptionInformation[1]) {
      static volatile LONG lines;

      if (InterlockedIncrement(&lines) <= 300)
         d3d9_log("%.200s", (const char *)info->ExceptionRecord->ExceptionInformation[1]);
      return EXCEPTION_CONTINUE_SEARCH;
   }
#if !defined(_WIN64)
   if (code == 0x80000003u && d3d9_trap_hit(info))
      return EXCEPTION_CONTINUE_EXECUTION;
#endif
   /* Errors only (not C++ throws or breakpoints). */
   if (code < 0xC0000000u || code == 0xE06D7363u)
      return EXCEPTION_CONTINUE_SEARCH;
   if (InterlockedExchange(&reported, 1) != 0)
      return EXCEPTION_CONTINUE_SEARCH;
   nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   top = base + nt->OptionalHeader.SizeOfImage;
   d3d9_where(info->ExceptionRecord->ExceptionAddress, line, sizeof line);
   d3d9_log("exception 0x%08lx at %s in thread %lu, access %p", (unsigned long)code, line,
            (unsigned long)GetCurrentThreadId(),
            info->ExceptionRecord->NumberParameters > 1
               ? (void *)info->ExceptionRecord->ExceptionInformation[1]
               : NULL);
   {
      const BYTE *code = info->ExceptionRecord->ExceptionAddress;
      char hex[80];
      SIZE_T got = 0;
      BYTE bytes[16];
      int h = 0;

      if (ReadProcessMemory(GetCurrentProcess(), code, bytes, sizeof bytes, &got)) {
         for (SIZE_T i = 0; i < got; i++)
            h += _snprintf(hex + h, sizeof hex - 1 - h, "%02x", bytes[i]);
         hex[h] = 0;
         d3d9_log("  code: %s", hex);
      }
#if defined(_WIN64)
      d3d9_log("  rax %Ix rbx %Ix rcx %Ix rdx %Ix rsi %Ix rdi %Ix rbp %Ix rsp %Ix",
               (size_t)info->ContextRecord->Rax, (size_t)info->ContextRecord->Rbx,
               (size_t)info->ContextRecord->Rcx, (size_t)info->ContextRecord->Rdx,
               (size_t)info->ContextRecord->Rsi, (size_t)info->ContextRecord->Rdi,
               (size_t)info->ContextRecord->Rbp, (size_t)info->ContextRecord->Rsp);
#else
      d3d9_log("  eax %Ix ebx %Ix ecx %Ix edx %Ix esi %Ix edi %Ix ebp %Ix esp %Ix",
               (size_t)info->ContextRecord->Eax, (size_t)info->ContextRecord->Ebx,
               (size_t)info->ContextRecord->Ecx, (size_t)info->ContextRecord->Edx,
               (size_t)info->ContextRecord->Esi, (size_t)info->ContextRecord->Edi,
               (size_t)info->ContextRecord->Ebp, (size_t)info->ContextRecord->Esp);
#endif
   }
#if !defined(_WIN64)
   {
      /* What the registers that hold pointers point at, two levels deep: a
       * fault inside the runtime is about one of its own structures. */
      const struct {
         const char *name;
         ULONG_PTR value;
      } regs[] = {{"edi", info->ContextRecord->Edi},
                  {"esi", info->ContextRecord->Esi},
                  {"ebx", info->ContextRecord->Ebx},
                  {"ecx", info->ContextRecord->Ecx}};

      for (unsigned r = 0; r < 4; r++) {
         ULONG_PTR words[10], inner[8];
         SIZE_T got = 0;

         if (regs[r].value < 0x10000 ||
             !ReadProcessMemory(GetCurrentProcess(), (const void *)regs[r].value, words,
                                sizeof words, &got) ||
             got != sizeof words)
            continue;
         d3d9_log("  [%s]: %Ix %Ix %Ix %Ix %Ix %Ix %Ix %Ix %Ix %Ix", regs[r].name, words[0],
                  words[1], words[2], words[3], words[4], words[5], words[6], words[7],
                  words[8], words[9]);
         for (unsigned w = 0; w < 6 && r == 0; w++) {
            if (words[w] < 0x10000 ||
                !ReadProcessMemory(GetCurrentProcess(), (const void *)words[w], inner,
                                   sizeof inner, &got) ||
                got != sizeof inner)
               continue;
            d3d9_log("    [%s+%u]: %Ix %Ix %Ix %Ix %Ix %Ix %Ix %Ix", regs[r].name, w * 4,
                     inner[0], inner[1], inner[2], inner[3], inner[4], inner[5], inner[6],
                     inner[7]);
         }
      }
   }
#endif
   d3d9_print_rings();
#if !defined(_WIN64)
   d3d9_print_runtime_errors();
   {
      /* The stack's words by module: return addresses mostly, some stale. */
      ULONG_PTR *w = (ULONG_PTR *)info->ContextRecord->Esp;
      ULONG_PTR *end = (ULONG_PTR *)((NT_TIB *)NtCurrentTeb())->StackBase;
      char where[140];
      unsigned listed = 0;
      int a = 0;

      for (; w < end && listed < 36; w++) {
         HMODULE module = NULL;

         if (*w < 0x10000 ||
             !GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                 (LPCSTR)*w, &module) ||
             !module)
            continue;
         d3d9_where((const void *)*w, where, sizeof where);
         a += _snprintf(line + a, sizeof line - 1 - a, " %s", where);
         listed++;
         if (a > 110) {
            line[a] = 0;
            d3d9_log("  words:%s", line);
            a = 0;
         }
      }
      if (a) {
         line[a] = 0;
         d3d9_log("  words:%s", line);
      }
   }
#endif
#if defined(_WIN64)
   {
      /* The callers, unwound: whose call it was, in whichever module. */
      CONTEXT context = *info->ContextRecord;

      for (unsigned frame = 0; frame < 28 && context.Rip; frame++) {
         DWORD64 image = 0, establisher = 0;
         PVOID handler_data = NULL;
         PRUNTIME_FUNCTION function = RtlLookupFunctionEntry(context.Rip, &image, NULL);

         if (frame) {
            d3d9_where((const void *)context.Rip, line, sizeof line);
            d3d9_log("  from %s", line);
         }
         if (function) {
            RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, context.Rip, function, &context,
                             &handler_data, &establisher, NULL);
         } else {
            if (context.Rsp < (DWORD64)info->ContextRecord->Rsp ||
                context.Rsp >= (DWORD64)((NT_TIB *)NtCurrentTeb())->StackBase)
               break;
            context.Rip = *(DWORD64 *)context.Rsp;
            context.Rsp += 8;
         }
      }
   }
#endif
#if defined(_WIN64)
   word = (ULONG_PTR *)info->ContextRecord->Rsp;
#else
   word = (ULONG_PTR *)info->ContextRecord->Esp;
#endif
   limit = (ULONG_PTR *)((NT_TIB *)NtCurrentTeb())->StackBase;
   for (; word < limit && found < 40; word++) {
      if (*word < base || *word >= top)
         continue;
      at += _snprintf(line + at, sizeof line - 1 - at, " 0x%lx", (unsigned long)(*word - base));
      found++;
      if (at > 150) {
         line[at] = 0;
         d3d9_log("  stack:%s", line);
         at = 0;
      }
   }
   if (at) {
      line[at] = 0;
      d3d9_log("  stack:%s", line);
   }
   return EXCEPTION_CONTINUE_SEARCH;
}

#if defined(_M_IX86)
/* A stack cookie found changed (a buffer on the stack overrun, or a callee
 * that popped the wrong number of arguments): the C runtime's own report ends
 * the process with a fast-fail no handler sees. Ours, which the linker takes
 * instead, first says where: the stack's words that point into this DLL, the
 * first of them in the function whose cookie it was. */
__declspec(noreturn) void __cdecl
__report_gsfailure(void)
{
   ULONG_PTR base = (ULONG_PTR)crash_module, top, *word, *limit, here = 0;
   IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   char line[220];
   int at = 0;
   unsigned found = 0;

   top = base + nt->OptionalHeader.SizeOfImage;
   word = &here;
   limit = (ULONG_PTR *)((NT_TIB *)NtCurrentTeb())->StackBase;
   d3d9_log("a stack cookie changed in thread %lu", (unsigned long)GetCurrentThreadId());
   for (; word < limit && found < 30; word++) {
      if (*word < base || *word >= top)
         continue;
      at += _snprintf(line + at, sizeof line - 1 - at, " 0x%lx", (unsigned long)(*word - base));
      found++;
      if (at > 150) {
         line[at] = 0;
         d3d9_log("  stack:%s", line);
         at = 0;
      }
   }
   if (at) {
      line[at] = 0;
      d3d9_log("  stack:%s", line);
   }
   __fastfail(2 /* FAST_FAIL_STACK_COOKIE_CHECK_FAILURE */);
}

/* The rest of what the C runtime keeps in the same object as its report, so
 * the linker takes ours whole and not both. */
__declspec(noreturn) void __cdecl
__report_rangecheckfailure(void)
{
   d3d9_log("a range check failed in thread %lu", (unsigned long)GetCurrentThreadId());
   __fastfail(8 /* FAST_FAIL_RANGE_CHECK_FAILURE */);
}

__declspec(noreturn) void __cdecl
__report_securefailure(ULONG code)
{
   __fastfail(code);
}

__declspec(noreturn) void __cdecl
__report_securefailureEx(ULONG code, ULONG count, void **parameters)
{
   (void)count;
   (void)parameters;
   __fastfail(code);
}
#endif

/* --------------------------------------------------------------- sampler */

#pragma comment(lib, "ntdll.lib") /* NtQueryObject */

/* Bring-up: with ENTANGLED_D3D9_SAMPLE=1, where the thread that last called
 * the driver is, every five seconds: where it executes, and the words of its
 * stack that point into a module (ours as offsets for the linker map). For a
 * program that neither draws nor dies. */
static volatile LONG sample_on;
static volatile LONG ddi_threads[8];
static volatile LONG count_presents, count_draws;

/* Said at the head of every device function: the threads that call the
 * driver (Windows 10's runtime plays batches of calls from a worker of its
 * own, while the program's thread waits for it). */
static void
d3d9_here(void)
{
   LONG id = (LONG)GetCurrentThreadId();

   for (unsigned i = 0; i < 8; i++) {
      LONG seen = ddi_threads[i];

      if (seen == id)
         return;
      if (!seen && InterlockedCompareExchange(&ddi_threads[i], id, 0) == 0)
         return;
   }
}

#define D3D9_HERE() \
   do { \
      call_ring[(unsigned)InterlockedIncrement(&call_at) & 63] = __func__; \
      if (calls_left) \
         d3d9_called(__func__); \
      if (sample_on) \
         d3d9_here(); \
   } while (0)

static void
d3d9_sample(DWORD id, unsigned samples)
{
   ULONG_PTR base = (ULONG_PTR)crash_module, top;
   IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);

   top = base + nt->OptionalHeader.SizeOfImage;
   {
      static ULONG_PTR words[1024];
      CONTEXT context;
      SIZE_T got = 0;
      char line[260], where[140];
      int at = 0;
      unsigned found = 0;
      static unsigned dumped;

      HANDLE sample_thread;

      sample_thread = id ? OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, FALSE, id) : NULL;
      if (!sample_thread) {
         d3d9_log("sample: thread %lu is gone", (unsigned long)id);
         return;
      }
      memset(&context, 0, sizeof context);
      context.ContextFlags = CONTEXT_CONTROL;
      if (SuspendThread(sample_thread) == (DWORD)-1) {
         CloseHandle(sample_thread);
         return;
      }
      if (!GetThreadContext(sample_thread, &context)) {
         ResumeThread(sample_thread);
         CloseHandle(sample_thread);
         return;
      }
#if defined(_WIN64)
      ReadProcessMemory(GetCurrentProcess(), (const void *)context.Rsp, words, sizeof words, &got);
      ResumeThread(sample_thread);
      CloseHandle(sample_thread);
      d3d9_where((const void *)context.Rip, where, sizeof where);
#else
      ReadProcessMemory(GetCurrentProcess(), (const void *)context.Esp, words, sizeof words, &got);
      ResumeThread(sample_thread);
      CloseHandle(sample_thread);
      d3d9_where((const void *)context.Eip, where, sizeof where);
#endif
      d3d9_log("sample: thread %lu at %s; %ld presents and %ld draws so far", (unsigned long)id,
               where, (long)count_presents, (long)count_draws);
      /* The top of the stack raw: a wait's arguments are among it. */
      if (got >= 8 * sizeof words[0])
         d3d9_log("  top: %Ix %Ix %Ix %Ix %Ix %Ix %Ix %Ix", (size_t)words[0], (size_t)words[1],
                  (size_t)words[2], (size_t)words[3], (size_t)words[4], (size_t)words[5],
                  (size_t)words[6], (size_t)words[7]);
      for (SIZE_T i = 0; i < 8 && i < got / sizeof words[0]; i++) {
         /* A small multiple of four may be a handle: say what kind. */
         if (words[i] >= 4 && words[i] < 0x10000 && !(words[i] & 3)) {
            struct {
               UNICODE_STRING name;
               WCHAR text[64];
            } type;
            ULONG length = 0;

            memset(&type, 0, sizeof type);
            if (NtQueryObject((HANDLE)words[i], 2 /* ObjectTypeInformation */, &type,
                              sizeof type, &length) >= 0 &&
                type.name.Buffer)
               d3d9_log("  handle 0x%Ix is a %.20ls", (size_t)words[i], type.name.Buffer);
         }
      }
      /* Every word of the stack that points into a module: return addresses
       * mostly, some stale. Ours as plain offsets, for the map. */
      for (SIZE_T i = 0; i < got / sizeof words[0] && found < 40; i++) {
         HMODULE module = NULL;

         if (words[i] < 0x10000)
            continue;
         if (words[i] >= base && words[i] < top) {
            at += _snprintf(line + at, sizeof line - 1 - at, " 0x%lx",
                            (unsigned long)(words[i] - base));
         } else if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                          GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                       (LPCSTR)words[i], &module) &&
                    module) {
            d3d9_where((const void *)words[i], where, sizeof where);
            at += _snprintf(line + at, sizeof line - 1 - at, " %s", where);
            /* The runtime's code before the address, a few times over: what
             * it called, for whoever has no copy of this build of it. */
            if (samples == 6 && dumped < 6 && module == GetModuleHandleA("d3d9.dll")) {
               char hex[200];
               const BYTE *code = (const BYTE *)words[i] - 80;
               int h = 0;

               dumped++;
               for (int row = 0; row < 2; row++) {
                  h = 0;
                  for (int b = 0; b < 48; b++)
                     h += _snprintf(hex + h, sizeof hex - 1 - h, "%02x", code[row * 48 + b]);
                  hex[h] = 0;
                  d3d9_log("  code d3d9+0x%Ix: %s",
                           (size_t)((ULONG_PTR)code + row * 48 - (ULONG_PTR)module), hex);
               }
            }
         } else {
            continue;
         }
         found++;
         if (at > 110) {
            line[at] = 0;
            d3d9_log("  sample stack:%s", line);
            at = 0;
         }
      }
      if (at) {
         line[at] = 0;
         d3d9_log("  sample stack:%s", line);
      }
   }
}

static DWORD WINAPI
d3d9_sampler(void *unused)
{
   (void)unused;
   for (unsigned samples = 0;; samples++) {
      Sleep(samples < 6 ? 5000 : 30000);
      for (unsigned i = 0; i < 8; i++) {
         if (ddi_threads[i])
            d3d9_sample((DWORD)ddi_threads[i], samples);
      }
   }
   return 0;
}

/* The profile: two tables of places and how often the thread was there. */
struct d3d9_hits {
   ULONG_PTR key;
   unsigned count;
};

static void
d3d9_hit(struct d3d9_hits *table, unsigned size, ULONG_PTR key)
{
   for (unsigned i = 0; i < size; i++) {
      if (table[i].key == key || !table[i].count) {
         table[i].key = key;
         table[i].count++;
         return;
      }
   }
}

static void
d3d9_top(const char *what, struct d3d9_hits *table, unsigned size, unsigned total, BOOL ours)
{
   ULONG_PTR base = (ULONG_PTR)crash_module;

   for (unsigned shown = 0; shown < 22; shown++) {
      unsigned best = 0;
      char where[140];

      for (unsigned i = 1; i < size; i++) {
         if (table[i].count > table[best].count)
            best = i;
      }
      if (!table[best].count)
         break;
      if (ours) {
         d3d9_log("profile %s: %4.1f%% +0x%Ix", what, 100.0 * table[best].count / total,
                  (size_t)(table[best].key - base));
      } else {
         d3d9_where((const void *)table[best].key, where, sizeof where);
         d3d9_log("profile %s: %4.1f%% %s", what, 100.0 * table[best].count / total, where);
      }
      table[best].count = 0;
   }
   memset(table, 0, size * sizeof *table);
}

static DWORD WINAPI
d3d9_profiler(void *unused)
{
   static struct d3d9_hits at[512], via[512], via2[512], caller[512];
   static ULONG_PTR words[96];
   /* The modules a wait goes through, which say nothing about who waits. */
   static const char *const plumbing[] = {"ntdll.dll", "KERNELBASE.dll", "KERNEL32.DLL",
                                          "win32u.dll", "wow64cpu.dll"};
   ULONG_PTR low[5], high[5];
   ULONG_PTR base = (ULONG_PTR)crash_module, top;
   IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   unsigned samples = 0;

   (void)unused;
   top = base + nt->OptionalHeader.SizeOfImage;
   for (unsigned i = 0; i < 5; i++) {
      const BYTE *module = (const BYTE *)GetModuleHandleA(plumbing[i]);

      low[i] = high[i] = 0;
      if (module) {
         const IMAGE_NT_HEADERS *h =
            (const IMAGE_NT_HEADERS *)(module + ((const IMAGE_DOS_HEADER *)module)->e_lfanew);

         low[i] = (ULONG_PTR)module;
         high[i] = low[i] + h->OptionalHeader.SizeOfImage;
      }
   }
   for (;;) {
      DWORD id = (DWORD)ddi_threads[0];
      HANDLE thread;
      CONTEXT context;
      SIZE_T got = 0;
      ULONG_PTR pc, sp;

      Sleep(2);
      /* The thread that draws is the one that called last among the few. */
      for (unsigned i = 1; i < 8; i++) {
         if (ddi_threads[i])
            id = (DWORD)ddi_threads[i];
      }
      thread = id ? OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, FALSE, id) : NULL;
      if (!thread)
         continue;
      memset(&context, 0, sizeof context);
      context.ContextFlags = CONTEXT_CONTROL;
      if (SuspendThread(thread) == (DWORD)-1 || !GetThreadContext(thread, &context)) {
         ResumeThread(thread);
         CloseHandle(thread);
         continue;
      }
#if defined(_WIN64)
      pc = context.Rip;
      sp = context.Rsp;
#else
      pc = context.Eip;
      sp = context.Esp;
#endif
      ReadProcessMemory(GetCurrentProcess(), (const void *)sp, words, sizeof words, &got);
      ResumeThread(thread);
      CloseHandle(thread);
      /* Where it is: in this DLL to 64 bytes, elsewhere to a page. */
      if (pc >= base && pc < top) {
         d3d9_hit(at, 512, pc & ~(ULONG_PTR)63);
      } else {
         d3d9_hit(at, 512, pc & ~(ULONG_PTR)4095);
         /* Who called: the first word that points into a module that is not
          * plumbing. In this DLL, that place and the next one up. */
         for (SIZE_T i = 0; i < got / sizeof words[0]; i++) {
            HMODULE module = NULL;
            BOOL skip = words[i] < 0x10000;

            for (unsigned k = 0; k < 5 && !skip; k++)
               skip = words[i] >= low[k] && words[i] < high[k];
            if (skip)
               continue;
            if (words[i] >= base && words[i] < top) {
               d3d9_hit(via, 512, words[i] & ~(ULONG_PTR)63);
               for (SIZE_T j = i + 1; j < got / sizeof words[0]; j++) {
                  if (words[j] >= base && words[j] < top &&
                      (words[j] & ~(ULONG_PTR)63) != (words[i] & ~(ULONG_PTR)63)) {
                     d3d9_hit(via2, 512, words[j] & ~(ULONG_PTR)63);
                     break;
                  }
               }
               break;
            }
            if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                      GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                   (LPCSTR)words[i], &module) &&
                module) {
               d3d9_hit(caller, 512, words[i] & ~(ULONG_PTR)4095);
               break;
            }
         }
      }
      if (++samples == 4000) {
         d3d9_log("profile: 4000 samples of thread %lu", (unsigned long)id);
         d3d9_top("via2", via2, 512, samples, TRUE);
         d3d9_top("caller", caller, 512, samples, FALSE);
         for (unsigned pass = 0; pass < 2; pass++) {
            /* This DLL's places as offsets (for the map), the others by name. */
            struct d3d9_hits ours[512], others[512];
            struct d3d9_hits *table = pass ? via : at;
            unsigned n = 0, m = 0;

            memset(ours, 0, sizeof ours);
            memset(others, 0, sizeof others);
            for (unsigned i = 0; i < 512; i++) {
               if (!table[i].count)
                  continue;
               if (table[i].key >= base && table[i].key < top)
                  ours[n++] = table[i];
               else
                  others[m++] = table[i];
            }
            d3d9_top(pass ? "via" : "in", ours, 512, samples, TRUE);
            if (!pass)
               d3d9_top("in", others, 512, samples, FALSE);
            memset(table, 0, sizeof at);
         }
         samples = 0;
      }
   }
   return 0;
}

static void
d3d9_sample_this_thread(void)
{
   char profile[4] = "";

   GetEnvironmentVariableA("ENTANGLED_D3D9_PROFILE", profile, sizeof profile);
   if (profile[0] == '1' && InterlockedExchange(&sample_on, 1) == 0) {
      HANDLE thread = CreateThread(NULL, 0, d3d9_profiler, NULL, 0, NULL);

      d3d9_here();
      if (thread)
         CloseHandle(thread);
      return;
   }
   char want[4] = "";

   if (sample_on)
      return;
   GetEnvironmentVariableA("ENTANGLED_D3D9_SAMPLE", want, sizeof want);
   if (want[0] != '1')
      return;
   if (InterlockedExchange(&sample_on, 1) == 0) {
      HANDLE thread = CreateThread(NULL, 0, d3d9_sampler, NULL, 0, NULL);

      d3d9_here();
      if (thread)
         CloseHandle(thread);
   }
}

/* ---------------------------------------------------------------- stubs */

static volatile LONG stub_said[160];

static HRESULT
d3d9_stub(unsigned index, const char *name)
{
   if (calls_left)
      d3d9_called(name);
   if (index < 160 && InterlockedExchange(&stub_said[index], 1) == 0)
      d3d9_log("stub: %s", name);
   return S_OK;
}

#include "entangled_d3d9_stubs.inc"

/* ----------------------------------------------------------------- caps */

/* What the adapter says it can do with a format: what Nine on Zink on a
 * desktop GPU does with each, written out (the runtime asks before there is a
 * device, and so before there is a screen to ask). A program that is refused a
 * format it takes for granted - a volume texture for colour correction, a
 * float render target - does not always look at the answer. */
#define OPS_SAMPLED                                                         \
   (FORMATOP_TEXTURE | FORMATOP_CUBETEXTURE | FORMATOP_VOLUMETEXTURE |      \
    FORMATOP_OFFSCREENPLAIN | FORMATOP_CONVERT_TO_ARGB | FORMATOP_VERTEXTEXTURE)
#define OPS_TARGET                                                          \
   (OPS_SAMPLED | FORMATOP_OFFSCREEN_RENDERTARGET |                         \
    FORMATOP_SAME_FORMAT_RENDERTARGET | FORMATOP_AUTOGENMIPMAP)
/* The three the desktop is made of: the runtime lets only these say they
 * are members of the ARGB group. */
#define OPS_COLOR                                                           \
   (OPS_TARGET | FORMATOP_SRGBREAD | FORMATOP_SRGBWRITE | FORMATOP_MEMBEROFGROUP_ARGB)
#define OPS_DISPLAY (OPS_COLOR | FORMATOP_DISPLAYMODE | FORMATOP_3DACCELERATION)
#define OPS_DEPTH \
   (FORMATOP_ZSTENCIL | FORMATOP_ZSTENCIL_WITH_ARBITRARY_COLOR_DEPTH)
/* Block-compressed: no volumes (Vulkan has none of them in three dimensions
 * everywhere). */
#define OPS_COMPRESSED                                                      \
   (FORMATOP_TEXTURE | FORMATOP_CUBETEXTURE | FORMATOP_OFFSCREENPLAIN |     \
    FORMATOP_CONVERT_TO_ARGB | FORMATOP_SRGBREAD)
#define OPS_BUMP (OPS_SAMPLED | FORMATOP_BUMPMAP)
#define D3D9_FOURCC(a, b, c, d) \
   ((D3DDDIFORMAT)((UINT)(a) | ((UINT)(b) << 8) | ((UINT)(c) << 16) | ((UINT)(d) << 24)))

static const FORMATOP d3d9_formats[] = {
   {D3DDDIFMT_X8R8G8B8, OPS_DISPLAY, 0, 0, 0},
   /* Never a display mode: d3d9.dll disables a driver that reports "an alpha
    * format with D3DFORMAT_OP_3DACCELERATION" (its words). */
   {D3DDDIFMT_A8R8G8B8, OPS_COLOR | FORMATOP_SAME_FORMAT_UP_TO_ALPHA_RENDERTARGET, 0, 0, 0},
   /* A display mode too, though the kernel driver has none of 16 bits: the
    * runtime looks through this list for a 16-bit display format (this one or
    * X1R5G5B5), and a driver with none is taken for one that cannot make the
    * front buffer of a full-screen device - which the runtime then goes
    * without, and dies of at the first present. */
   {D3DDDIFMT_R5G6B5, OPS_DISPLAY & ~FORMATOP_VOLUMETEXTURE, 0, 0, 0},
   {D3DDDIFMT_A2B10G10R10, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_A2R10G10B10, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_A16B16G16R16, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_G16R16, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_R16F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_G16R16F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_A16B16G16R16F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_R32F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_G32R32F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_A32B32G32R32F, OPS_TARGET, 0, 0, 0},
   {D3DDDIFMT_A8, OPS_SAMPLED, 0, 0, 0},
   {D3DDDIFMT_L8, OPS_SAMPLED, 0, 0, 0},
   {D3DDDIFMT_A8L8, OPS_SAMPLED, 0, 0, 0},
   {D3DDDIFMT_L16, OPS_SAMPLED, 0, 0, 0},
   {D3DDDIFMT_V8U8, OPS_BUMP, 0, 0, 0},
   {D3DDDIFMT_Q8W8V8U8, OPS_BUMP, 0, 0, 0},
   {D3DDDIFMT_V16U16, OPS_BUMP, 0, 0, 0},
   {D3DDDIFMT_DXT1, OPS_COMPRESSED, 0, 0, 0},
   {D3DDDIFMT_DXT2, OPS_COMPRESSED, 0, 0, 0},
   {D3DDDIFMT_DXT3, OPS_COMPRESSED, 0, 0, 0},
   {D3DDDIFMT_DXT4, OPS_COMPRESSED, 0, 0, 0},
   {D3DDDIFMT_DXT5, OPS_COMPRESSED, 0, 0, 0},
   {D3D9_FOURCC('A', 'T', 'I', '1'), OPS_COMPRESSED & ~FORMATOP_SRGBREAD, 0, 0, 0},
   {D3D9_FOURCC('A', 'T', 'I', '2'), OPS_COMPRESSED & ~FORMATOP_SRGBREAD, 0, 0, 0},
   /* Depth: as buffers, and as textures a shader compares against (what
    * Direct3D 9 drivers call hardware shadow maps). */
   {D3DDDIFMT_D16, OPS_DEPTH | FORMATOP_TEXTURE, 0, 0, 0},
   {D3DDDIFMT_D24X8, OPS_DEPTH | FORMATOP_TEXTURE, 0, 0, 0},
   {D3DDDIFMT_D24S8, OPS_DEPTH | FORMATOP_TEXTURE, 0, 0, 0},
   {D3D9_FOURCC('I', 'N', 'T', 'Z'), OPS_DEPTH | FORMATOP_TEXTURE, 0, 0, 0},
   /* A render target that is not there, for drawing depth alone. */
   {D3D9_FOURCC('N', 'U', 'L', 'L'),
    FORMATOP_TEXTURE | FORMATOP_OFFSCREEN_RENDERTARGET | FORMATOP_SAME_FORMAT_RENDERTARGET, 0, 0,
    0},
};

static const D3DDDIQUERYTYPE d3d9_queries[] = {
   D3DDDIQUERYTYPE_EVENT,
   D3DDDIQUERYTYPE_OCCLUSION,
};

/* Direct3D 9's caps, as a shader model 3 card of 2008 would give them. */
static void
d3d9_caps9(D3DCAPS9 *caps)
{
   memset(caps, 0, sizeof *caps);
   caps->DeviceType = D3DDEVTYPE_HAL;
   caps->Caps2 = D3DCAPS2_CANAUTOGENMIPMAP | D3DCAPS2_DYNAMICTEXTURES |
                 D3DCAPS2_FULLSCREENGAMMA | D3DCAPS2_CANSHARERESOURCE;
   caps->Caps3 = D3DCAPS3_ALPHA_FULLSCREEN_FLIP_OR_DISCARD |
                 D3DCAPS3_COPY_TO_VIDMEM | D3DCAPS3_COPY_TO_SYSTEMMEM |
                 D3DCAPS3_LINEAR_TO_SRGB_PRESENTATION;
   caps->PresentationIntervals = D3DPRESENT_INTERVAL_ONE |
                                 D3DPRESENT_INTERVAL_IMMEDIATE;
   caps->CursorCaps = D3DCURSORCAPS_COLOR;
   caps->DevCaps = D3DDEVCAPS_CANBLTSYSTONONLOCAL | D3DDEVCAPS_CANRENDERAFTERFLIP |
                   D3DDEVCAPS_DRAWPRIMITIVES2 | D3DDEVCAPS_DRAWPRIMITIVES2EX |
                   D3DDEVCAPS_DRAWPRIMTLVERTEX | D3DDEVCAPS_EXECUTESYSTEMMEMORY |
                   D3DDEVCAPS_EXECUTEVIDEOMEMORY | D3DDEVCAPS_HWRASTERIZATION |
                   D3DDEVCAPS_HWTRANSFORMANDLIGHT | D3DDEVCAPS_PUREDEVICE |
                   D3DDEVCAPS_TEXTURENONLOCALVIDMEM | D3DDEVCAPS_TEXTUREVIDEOMEMORY |
                   D3DDEVCAPS_TLVERTEXSYSTEMMEMORY | D3DDEVCAPS_TLVERTEXVIDEOMEMORY;
   caps->PrimitiveMiscCaps = D3DPMISCCAPS_MASKZ | D3DPMISCCAPS_CULLNONE |
                             D3DPMISCCAPS_CULLCW | D3DPMISCCAPS_CULLCCW |
                             D3DPMISCCAPS_COLORWRITEENABLE |
                             D3DPMISCCAPS_CLIPPLANESCALEDPOINTS |
                             D3DPMISCCAPS_TSSARGTEMP | D3DPMISCCAPS_BLENDOP |
                             D3DPMISCCAPS_INDEPENDENTWRITEMASKS |
                             D3DPMISCCAPS_PERSTAGECONSTANT | D3DPMISCCAPS_FOGANDSPECULARALPHA |
                             D3DPMISCCAPS_SEPARATEALPHABLEND |
                             D3DPMISCCAPS_MRTPOSTPIXELSHADERBLENDING |
                             D3DPMISCCAPS_FOGVERTEXCLAMPED |
                             /* A DDI-only bit (d3dhal.h), and not optional: d3d9.dll
                              * holds a shader model 2+ driver to a list of minimum
                              * caps, this one among them, and one that falls short has
                              * its DevCaps zeroed and its formats cut down to display
                              * modes - without a word, as D3DERR_NOTAVAILABLE from
                              * every call (ADR-0011 has the whole list). */
                             D3DPMISCCAPS_FOGINFVF;
   caps->RasterCaps = D3DPRASTERCAPS_ANISOTROPY | D3DPRASTERCAPS_COLORPERSPECTIVE |
                      D3DPRASTERCAPS_DITHER | D3DPRASTERCAPS_DEPTHBIAS |
                      D3DPRASTERCAPS_FOGRANGE | D3DPRASTERCAPS_FOGTABLE |
                      D3DPRASTERCAPS_FOGVERTEX | D3DPRASTERCAPS_MIPMAPLODBIAS |
                      D3DPRASTERCAPS_SCISSORTEST | D3DPRASTERCAPS_SLOPESCALEDEPTHBIAS |
                      D3DPRASTERCAPS_WFOG | D3DPRASTERCAPS_ZFOG | D3DPRASTERCAPS_ZTEST;
   caps->ZCmpCaps = 0xff;
   caps->AlphaCmpCaps = 0xff;
   caps->SrcBlendCaps = 0x3fff;
   caps->DestBlendCaps = 0x3fff;
   caps->ShadeCaps = D3DPSHADECAPS_COLORGOURAUDRGB | D3DPSHADECAPS_SPECULARGOURAUDRGB |
                     D3DPSHADECAPS_ALPHAGOURAUDBLEND | D3DPSHADECAPS_FOGGOURAUD;
   caps->TextureCaps = D3DPTEXTURECAPS_ALPHA | D3DPTEXTURECAPS_ALPHAPALETTE |
                       D3DPTEXTURECAPS_PERSPECTIVE | D3DPTEXTURECAPS_PROJECTED |
                       D3DPTEXTURECAPS_CUBEMAP | D3DPTEXTURECAPS_VOLUMEMAP |
                       D3DPTEXTURECAPS_MIPMAP | D3DPTEXTURECAPS_MIPVOLUMEMAP |
                       D3DPTEXTURECAPS_MIPCUBEMAP | D3DPTEXTURECAPS_TEXREPEATNOTSCALEDBYSIZE;
   caps->TextureFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR |
                             D3DPTFILTERCAPS_MINFANISOTROPIC | D3DPTFILTERCAPS_MIPFPOINT |
                             D3DPTFILTERCAPS_MIPFLINEAR | D3DPTFILTERCAPS_MAGFPOINT |
                             D3DPTFILTERCAPS_MAGFLINEAR | D3DPTFILTERCAPS_MAGFANISOTROPIC;
   caps->CubeTextureFilterCaps = caps->TextureFilterCaps;
   caps->VolumeTextureFilterCaps = caps->TextureFilterCaps;
   caps->TextureAddressCaps = D3DPTADDRESSCAPS_WRAP | D3DPTADDRESSCAPS_MIRROR |
                              D3DPTADDRESSCAPS_CLAMP | D3DPTADDRESSCAPS_BORDER |
                              D3DPTADDRESSCAPS_INDEPENDENTUV | D3DPTADDRESSCAPS_MIRRORONCE;
   caps->VolumeTextureAddressCaps = caps->TextureAddressCaps;
   caps->LineCaps = D3DLINECAPS_TEXTURE | D3DLINECAPS_ZTEST | D3DLINECAPS_BLEND |
                    D3DLINECAPS_ALPHACMP | D3DLINECAPS_FOG;
   caps->MaxTextureWidth = 8192;
   caps->MaxTextureHeight = 8192;
   caps->MaxVolumeExtent = 2048;
   caps->MaxTextureRepeat = 8192;
   caps->MaxTextureAspectRatio = 8192;
   caps->MaxAnisotropy = 16;
   caps->MaxVertexW = 1e10f;
   caps->GuardBandLeft = -32768.0f;
   caps->GuardBandTop = -32768.0f;
   caps->GuardBandRight = 32768.0f;
   caps->GuardBandBottom = 32768.0f;
   caps->StencilCaps = 0x1ff;
   caps->FVFCaps = D3DFVFCAPS_PSIZE | 8;
   caps->TextureOpCaps = 0x03ffffff;
   caps->MaxTextureBlendStages = 8;
   caps->MaxSimultaneousTextures = 8;
   caps->VertexProcessingCaps = D3DVTXPCAPS_TEXGEN | D3DVTXPCAPS_MATERIALSOURCE7 |
                                D3DVTXPCAPS_DIRECTIONALLIGHTS | D3DVTXPCAPS_POSITIONALLIGHTS |
                                D3DVTXPCAPS_LOCALVIEWER | D3DVTXPCAPS_TEXGEN_SPHEREMAP;
   caps->MaxActiveLights = 8;
   caps->MaxUserClipPlanes = 6;
   caps->MaxVertexBlendMatrices = 4;
   caps->MaxVertexBlendMatrixIndex = 0;
   caps->MaxPointSize = 256.0f;
   caps->MaxPrimitiveCount = 0x555555;
   caps->MaxVertexIndex = 0xffffff;
   caps->MaxStreams = 16;
   caps->MaxStreamStride = 508;
   caps->VertexShaderVersion = D3DVS_VERSION(3, 0);
   caps->MaxVertexShaderConst = 256;
   caps->PixelShaderVersion = D3DPS_VERSION(3, 0);
   caps->PixelShader1xMaxValue = 65504.0f;
   caps->DevCaps2 = D3DDEVCAPS2_STREAMOFFSET | D3DDEVCAPS2_VERTEXELEMENTSCANSHARESTREAMOFFSET |
                    D3DDEVCAPS2_CAN_STRETCHRECT_FROM_TEXTURES;
   caps->MasterAdapterOrdinal = 0;
   caps->AdapterOrdinalInGroup = 0;
   caps->NumberOfAdaptersInGroup = 1;
   caps->DeclTypes = D3DDTCAPS_UBYTE4 | D3DDTCAPS_UBYTE4N | D3DDTCAPS_SHORT2N |
                     D3DDTCAPS_SHORT4N | D3DDTCAPS_USHORT2N | D3DDTCAPS_USHORT4N |
                     D3DDTCAPS_FLOAT16_2 | D3DDTCAPS_FLOAT16_4;
   caps->NumSimultaneousRTs = 4;
   caps->StretchRectFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR |
                                 D3DPTFILTERCAPS_MAGFPOINT | D3DPTFILTERCAPS_MAGFLINEAR;
   caps->VS20Caps.Caps = D3DVS20CAPS_PREDICATION;
   caps->VS20Caps.DynamicFlowControlDepth = 24;
   caps->VS20Caps.NumTemps = 32;
   caps->VS20Caps.StaticFlowControlDepth = 4;
   caps->PS20Caps.Caps = D3DPS20CAPS_ARBITRARYSWIZZLE | D3DPS20CAPS_GRADIENTINSTRUCTIONS |
                         D3DPS20CAPS_PREDICATION | D3DPS20CAPS_NODEPENDENTREADLIMIT |
                         D3DPS20CAPS_NOTEXINSTRUCTIONLIMIT;
   caps->PS20Caps.DynamicFlowControlDepth = 24;
   caps->PS20Caps.NumTemps = 32;
   caps->PS20Caps.StaticFlowControlDepth = 4;
   caps->PS20Caps.NumInstructionSlots = 512;
   caps->VertexTextureFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MAGFPOINT;
   caps->MaxVShaderInstructionsExecuted = 65535;
   caps->MaxPShaderInstructionsExecuted = 65535;
   caps->MaxVertexShader30InstructionSlots = 32768;
   caps->MaxPixelShader30InstructionSlots = 32768;
}

static const char *
caps_name(D3DDDICAPS_TYPE type)
{
   switch (type) {
   case D3DDDICAPS_DDRAW: return "DDRAW";
   case D3DDDICAPS_DDRAW_MODE_SPECIFIC: return "DDRAW_MODE_SPECIFIC";
   case D3DDDICAPS_GETFORMATCOUNT: return "FORMATCOUNT";
   case D3DDDICAPS_GETFORMATDATA: return "FORMATDATA";
   case D3DDDICAPS_GETMULTISAMPLEQUALITYLEVELS: return "MULTISAMPLEQUALITYLEVELS";
   case D3DDDICAPS_GETD3DQUERYCOUNT: return "QUERYCOUNT";
   case D3DDDICAPS_GETD3DQUERYDATA: return "QUERYDATA";
   case D3DDDICAPS_GETD3D3CAPS: return "D3D3CAPS";
   case D3DDDICAPS_GETD3D5CAPS: return "D3D5CAPS";
   case D3DDDICAPS_GETD3D6CAPS: return "D3D6CAPS";
   case D3DDDICAPS_GETD3D7CAPS: return "D3D7CAPS";
   case D3DDDICAPS_GETD3D8CAPS: return "D3D8CAPS";
   case D3DDDICAPS_GETD3D9CAPS: return "D3D9CAPS";
   case D3DDDICAPS_GETGAMMARAMPCAPS: return "GAMMARAMPCAPS";
   default: return "?";
   }
}

static HRESULT APIENTRY
d3d9_get_caps(HANDLE adapter, const D3DDDIARG_GETCAPS *arg)
{
   HRESULT result = S_OK;

   (void)adapter;
   switch (arg->Type) {
   case D3DDDICAPS_DDRAW: {
      DDRAW_CAPS *caps = arg->pData;

      if (arg->DataSize < sizeof *caps)
         return E_INVALIDARG;
      memset(caps, 0, sizeof *caps);
      caps->Caps2 = DDRAW_CAPS2_FLIPINTERVAL | DDRAW_CAPS2_FLIPNOVSYNC |
                    DDRAW_CAPS2_DYNAMICTEXTURES;
      break;
   }
   case D3DDDICAPS_DDRAW_MODE_SPECIFIC: {
      DDRAW_MODE_SPECIFIC_CAPS *caps = arg->pData;
      UINT head;

      if (arg->DataSize < sizeof *caps)
         return E_INVALIDARG;
      head = caps->Head;
      memset(caps, 0, sizeof *caps);
      caps->Head = head;
      break;
   }
   case D3DDDICAPS_GETFORMATCOUNT:
      if (arg->DataSize < sizeof(UINT))
         return E_INVALIDARG;
      *(UINT *)arg->pData = ARRAYSIZE(d3d9_formats);
      break;
   case D3DDDICAPS_GETFORMATDATA:
      if (arg->DataSize < sizeof d3d9_formats)
         return E_INVALIDARG;
      memcpy(arg->pData, d3d9_formats, sizeof d3d9_formats);
      break;
   case D3DDDICAPS_GETMULTISAMPLEQUALITYLEVELS: {
      DDIMULTISAMPLEQUALITYLEVELSDATA *levels = arg->pData;

      if (arg->DataSize < sizeof *levels)
         return E_INVALIDARG;
      levels->QualityLevels = levels->MsType == D3DDDIMULTISAMPLE_NONE ? 1 : 0;
      break;
   }
   case D3DDDICAPS_GETD3DQUERYCOUNT:
      if (arg->DataSize < sizeof(UINT))
         return E_INVALIDARG;
      *(UINT *)arg->pData = ARRAYSIZE(d3d9_queries);
      break;
   case D3DDDICAPS_GETD3DQUERYDATA:
      if (arg->DataSize < sizeof d3d9_queries)
         return E_INVALIDARG;
      memcpy(arg->pData, d3d9_queries, sizeof d3d9_queries);
      break;
   case D3DDDICAPS_GETD3D8CAPS:
   case D3DDDICAPS_GETD3D9CAPS: {
      /* Direct3D 8's structure is the head of Direct3D 9's. */
      D3DCAPS9 caps;

      d3d9_caps9(&caps);
      memset(arg->pData, 0, arg->DataSize);
      memcpy(arg->pData, &caps, min(arg->DataSize, sizeof caps));
      break;
   }
   case D3DDDICAPS_GETD3D3CAPS: {
      /* The DirectDraw-era description: a size, and that this is hardware. */
      D3DHAL_GLOBALDRIVERDATA *data = arg->pData;

      memset(arg->pData, 0, arg->DataSize);
      if (arg->DataSize >= sizeof *data) {
         data->dwSize = sizeof *data;
         data->hwCaps.dwSize = sizeof data->hwCaps;
         data->hwCaps.dwFlags = D3DDD_COLORMODEL | D3DDD_DEVCAPS | D3DDD_TRICAPS |
                                D3DDD_LINECAPS | D3DDD_DEVICERENDERBITDEPTH |
                                D3DDD_DEVICEZBUFFERBITDEPTH;
         data->hwCaps.dcmColorModel = D3DCOLOR_RGB;
         data->hwCaps.dwDevCaps = D3DDEVCAPS_HWRASTERIZATION | D3DDEVCAPS_DRAWPRIMITIVES2EX |
                                  D3DDEVCAPS_DRAWPRIMITIVES2 | D3DDEVCAPS_DRAWPRIMTLVERTEX |
                                  D3DDEVCAPS_EXECUTEVIDEOMEMORY | D3DDEVCAPS_TLVERTEXVIDEOMEMORY |
                                  D3DDEVCAPS_TEXTUREVIDEOMEMORY | D3DDEVCAPS_HWTRANSFORMANDLIGHT;
         data->hwCaps.dwDeviceRenderBitDepth = DDBD_16 | DDBD_32;
         data->hwCaps.dwDeviceZBufferBitDepth = DDBD_16 | DDBD_24 | DDBD_32;
         data->dwNumVertices = 0;
         data->dwNumClipVertices = 0;
      }
      break;
   }
   case D3DDDICAPS_GETD3D5CAPS:
   case D3DDDICAPS_GETD3D6CAPS:
   case D3DDDICAPS_GETD3D7CAPS:
   case D3DDDICAPS_GETGAMMARAMPCAPS:
      memset(arg->pData, 0, arg->DataSize);
      break;
   case D3DDDICAPS_GETDECODEGUIDCOUNT:
   case D3DDDICAPS_GETVIDEOPROCESSORDEVICEGUIDCOUNT:
   case D3DDDICAPS_GETEXTENSIONGUIDCOUNT:
      /* No video decoding, processing or extensions. */
      if (arg->DataSize >= sizeof(UINT))
         *(UINT *)arg->pData = 0;
      break;
   default:
      memset(arg->pData, 0, arg->DataSize);
      result = E_NOTIMPL;
      break;
   }
      d3d9_log("caps %u %s, %u bytes: 0x%08lx", (unsigned)arg->Type,
               caps_name(arg->Type), arg->DataSize, (unsigned long)result);
   return result;
}

/* --------------------------------------------------------------- device */

struct d3d9_adapter {
   HANDLE runtime;
   D3DDDI_ADAPTERCALLBACKS callbacks;
};

struct d3d9_resource;

struct d3d9_device {
   HANDLE runtime;
   struct d3d9_adapter *adapter;
   D3DDDI_DEVICECALLBACKS callbacks;
   /* The kernel context presents are submitted on. */
   HANDLE context;
   /* Nine on a Gallium screen (d3d9_nine.c), or NULL where there is none:
    * the device then only answers, as in phase N0. */
   struct entangled_nine *nine;

   /* Stream 0 and the indices, when the runtime hands them over as memory of
    * the program's (SetStreamSourceUm, SetIndicesUm) and not as buffers. */
   const void *um_vertices;
   UINT stream0_stride;
   const void *um_indices;
   UINT um_index_size;
   /* The index buffer the program has set, to put back after a draw that
    * came with indices of its own. */
   struct d3d9_resource *indices;
   /* The vertex declaration the program has set. */
   struct d3d9_declaration *declaration;
   /* An index buffer of ours for such draws (DrawIndexedPrimitive2 over a
    * vertex buffer: indices in memory, vertices not). */
   void *scratch_indices;
   UINT scratch_size;
   /* Exclusive full screen: the size and format of the flip chain (what the
    * back buffer the runtime creates as a primary says; the front buffer it
    * opens says nothing), and when the kernel driver was last told that the
    * presents are on the host's plane. */
   UINT chain_width, chain_height;
   D3DDDIFORMAT chain_format;
   DWORD leased;
   BOOL on_plane;
};

/* One surface of a resource the runtime keeps in system memory: the driver
 * is told where it is, and reads it when it is copied somewhere. */
struct d3d9_sysmem {
   void *memory;
   UINT pitch, slice_pitch;
   UINT width, height, depth;
};

/* What the runtime calls a resource: one or more surfaces of one kind. */
struct d3d9_resource {
   HANDLE runtime;
   D3DDDIFORMAT format;
   D3DDDI_RESOURCEFLAGS flags;
   UINT surfaces;
   UINT width, height, depth;
   /* Nine's object for it and what it is, or NULL (a resource in system
    * memory that is not a buffer has none). */
   void *nine;
   enum entangled_nine_kind kind;
   UINT levels;
   /* System memory of the runtime's, one entry a surface; NULL for a resource
    * in video memory. A buffer there has a Nine buffer as well, brought up to
    * date when the runtime says it wrote (a lock that only notifies). */
   struct d3d9_sysmem *sysmem;
   UINT notified_offset, notified_size;
   /* What the lock promised about the range (D3DLOCK_NOOVERWRITE, _DISCARD),
    * for the copy into Nine's buffer to promise too. */
   UINT notified_flags;
   /* A dynamic buffer is drawn straight from the runtime's memory while it is
    * only ever stream 0 or the indices; once a draw needs Nine's copy (it
    * was bound as another stream), the copy is kept. */
   BOOL copied;
   /* A render target the runtime may present has an allocation of the kernel
    * driver's: a present is a copy from it, and it is what DWM is shown. */
   D3DKMT_HANDLE allocation;
   /* The frame on its way from the GPU to that allocation. */
   void *pixels;
   /* One of a full-screen device's flip chain: the back buffer the runtime
    * creates with the Primary flag, or the screen's shared primary, which it
    * opens (and which has nothing of Nine's until it is first drawn to). */
   BOOL chain, opened;
};

/* The kernel driver's escape that takes a frame for a present to copy on
 * (0x904; ADR-0010, the second pass): type and length, the allocation, a
 * buffer of ours and its size. */
struct d3d9_escape_write {
   unsigned short type;
   unsigned short length;
   UINT handle;
   unsigned long long buffer;
   unsigned long long size;
   char rest[216];
};

static NTSTATUS
d3d9_escape(struct d3d9_device *device, void *data, UINT size)
{
   D3DDDICB_ESCAPE escape;

   memset(&escape, 0, sizeof escape);
   escape.hDevice = device->runtime;
   escape.pPrivateDriverData = data;
   escape.PrivateDriverDataSize = size;
   escape.hContext = device->context;
   return device->callbacks.pfnEscapeCb(device->adapter->runtime, &escape);
}

/* Bring-up: with ENTANGLED_D3D9_TRACE=1, the first few hundred calls that
 * decide what a draw is (streams, declaration, shaders, the draws); with 2,
 * the calls that decide where drawing goes instead (targets, clears, copies,
 * presents, queries), up to a few thousand. */
static volatile LONG trace_left = -1;
static volatile LONG trace_frame_left;

static BOOL
d3d9_tracing(void)
{
   if (trace_left == -1) {
      char want[4] = "";

      GetEnvironmentVariableA("ENTANGLED_D3D9_TRACE", want, sizeof want);
      InterlockedExchange(&trace_frame_left, want[0] == '2' ? 4000 : 0);
      InterlockedExchange(&trace_left, want[0] == '1' ? 400 : 0);
   }
   return trace_left > 0 && InterlockedDecrement(&trace_left) >= 0;
}

static BOOL
d3d9_tracing_frames(void)
{
   if (trace_left == -1)
      d3d9_tracing();
   return trace_frame_left > 0 && InterlockedDecrement(&trace_frame_left) >= 0;
}

#define D3D9_FRAME(...)         \
   do {                         \
      if (note_on)              \
         d3d9_note(__VA_ARGS__); \
      if (d3d9_tracing_frames()) \
         d3d9_log(__VA_ARGS__); \
   } while (0)

#define D3D9_TRACE(...)         \
   do {                         \
      if (note_on)              \
         d3d9_note(__VA_ARGS__); \
      if (d3d9_tracing())       \
         d3d9_log(__VA_ARGS__); \
   } while (0)

/* A failure of Nine's, said once a call site (the runtime repeats itself). */
static HRESULT
d3d9_result(const char *what, long result, volatile LONG *said)
{
   if (result < 0 && InterlockedIncrement(said) <= 3)
      d3d9_log("%s failed: 0x%08lx", what, (unsigned long)result);
   return (HRESULT)result;
}

#define D3D9_CHECK(what, call)                                 \
   do {                                                        \
      static volatile LONG said_;                              \
      return d3d9_result(what, (call), &said_);                \
   } while (0)

/* ------------------------------------------------------------ resources */

/* An allocation the kernel driver will copy from at a present. Its private
 * data is the kernel driver's contract (viogpu_wddm_driver.h). */
static D3DKMT_HANDLE
d3d9_allocate(struct d3d9_device *device, HANDLE runtime_resource, UINT width, UINT height,
              BOOL alpha)
{
   VIOGPU_CREATE_RESOURCE_EXCHANGE resource_exchange;
   VIOGPU_CREATE_ALLOCATION_EXCHANGE allocation_exchange;
   D3DDDI_ALLOCATIONINFO info;
   D3DDDICB_ALLOCATE allocate;
   HRESULT hr;

   memset(&resource_exchange, 0, sizeof resource_exchange);
   memset(&allocation_exchange, 0, sizeof allocation_exchange);
   allocation_exchange.ResourceOptions.target = 2; /* PIPE_TEXTURE_2D */
   allocation_exchange.ResourceOptions.format = alpha ? 1 : 2; /* B8G8R8A8, B8G8R8X8 */
   allocation_exchange.ResourceOptions.width = width;
   allocation_exchange.ResourceOptions.height = height;
   allocation_exchange.ResourceOptions.depth = 1;
   allocation_exchange.ResourceOptions.array_size = 1;
   allocation_exchange.Size = (ULONGLONG)width * 4 * height;

   memset(&info, 0, sizeof info);
   info.pPrivateDriverData = &allocation_exchange;
   info.PrivateDriverDataSize = sizeof allocation_exchange;

   memset(&allocate, 0, sizeof allocate);
   allocate.pPrivateDriverData = &resource_exchange;
   allocate.PrivateDriverDataSize = sizeof resource_exchange;
   allocate.hResource = runtime_resource;
   allocate.NumAllocations = 1;
   allocate.pAllocationInfo = &info;
   hr = device->callbacks.pfnAllocateCb(device->runtime, &allocate);
   if (FAILED(hr) || !info.hAllocation) {
      d3d9_log("allocation %ux%u failed: 0x%08lx", width, height, (unsigned long)hr);
      return 0;
   }
   return info.hAllocation;
}

static void
d3d9_resource_free(struct d3d9_device *device, struct d3d9_resource *resource)
{
   if (resource->chain && device->on_plane && device->nine) {
      /* The chain goes (the device leaves the whole screen, or ends): the
       * plane back to the desktop. */
      entangled_nine_show(device->nine, NULL, 0, 0);
      device->on_plane = FALSE;
   }
   entangled_nine_release(resource->nine);
   if (resource->allocation && !resource->opened) {
      D3DDDICB_DEALLOCATE deallocate;

      memset(&deallocate, 0, sizeof deallocate);
      deallocate.hResource = resource->runtime;
      device->callbacks.pfnDeallocateCb(device->runtime, &deallocate);
   }
   free(resource->sysmem);
   free(resource->pixels);
   free(resource);
}

#if !defined(_WIN64)
static void
d3d9_dump_runtime_modes(void)
{
   static volatile LONG done;
   char want[4] = "";
   const BYTE *base = (const BYTE *)GetModuleHandleA("d3d9.dll");
   const IMAGE_NT_HEADERS *nt;
   const BYTE *text, *end;

   GetEnvironmentVariableA("ENTANGLED_D3D9_MODES", want, sizeof want);
   if (want[0] != '1' || !base || InterlockedExchange(&done, 1))
      return;
   nt = (const IMAGE_NT_HEADERS *)(base + ((const IMAGE_DOS_HEADER *)base)->e_lfanew);
   text = base + nt->OptionalHeader.BaseOfCode;
   end = text + nt->OptionalHeader.SizeOfCode - 16;
   for (const BYTE *at = text; at < end; at++) {
      const UINT *table;
      UINT first, second;

      /* 8b 0c c5 <table>  8b 04 c5 <table + 4> */
      if (at[0] != 0x8b || at[1] != 0x0c || at[2] != 0xc5 || at[7] != 0x8b || at[8] != 0x04 ||
          at[9] != 0xc5)
         continue;
      memcpy(&first, at + 3, 4);
      memcpy(&second, at + 10, 4);
      if (second != first + 4)
         continue;
      table = (const UINT *)(ULONG_PTR)first;
      d3d9_log("runtime mode table at d3d9+0x%Ix (read at +0x%Ix)",
               (size_t)((ULONG_PTR)table - (ULONG_PTR)base), (size_t)(at - base));
      for (unsigned source = 0; source < 3; source++) {
         UINT count = table[source * 2];
         const UINT *modes = (const UINT *)(ULONG_PTR)table[source * 2 + 1];

         d3d9_log("  source %u: %u modes at %p", source, count, (const void *)modes);
         if (!modes || count > 400)
            continue;
         for (UINT i = 0; i < count; i++) {
            const UINT *m = modes + i * 11;

            if (m[0] != 1920 && i > 3)
               continue;
            d3d9_log("    %ux%u format %u, %u Hz (%u/%u), scanline %u, orientation %u, fixed %u, "
                     "flags 0x%x, +0x28 0x%x",
                     m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8], m[9], m[10]);
         }
      }
      return;
   }
   d3d9_log("runtime mode table: not found");
}

static void
d3d9_dump_runtime_frames(const char *what)
{
   static volatile LONG left = 3;
   char want[4] = "";
   const BYTE *base = (const BYTE *)GetModuleHandleA("d3d9.dll");
   const IMAGE_NT_HEADERS *nt;
   ULONG_PTR low, high, here = 0, *word, *limit;
   unsigned found = 0;

   GetEnvironmentVariableA("ENTANGLED_D3D9_MODES", want, sizeof want);
   if (want[0] != '1' || !base || InterlockedDecrement(&left) < 0)
      return;
   nt = (const IMAGE_NT_HEADERS *)(base + ((const IMAGE_DOS_HEADER *)base)->e_lfanew);
   low = (ULONG_PTR)base + nt->OptionalHeader.BaseOfCode;
   high = low + nt->OptionalHeader.SizeOfCode;
   limit = (ULONG_PTR *)((NT_TIB *)NtCurrentTeb())->StackBase;
   d3d9_print_runtime_errors();
   d3d9_log("runtime frames above %s:", what);
   for (word = &here; word < limit && found < 14; word++) {
      const BYTE *code;
      char hex[80];
      int h = 0;

      if (*word < low + 16 || *word >= high)
         continue;
      code = (const BYTE *)*word - 14;
      for (int b = 0; b < 22; b++)
         h += _snprintf(hex + h, sizeof hex - 1 - h, "%02x", code[b]);
      hex[h] = 0;
      d3d9_log("  d3d9+0x%Ix: %s", (size_t)(*word - (ULONG_PTR)base), hex);
      found++;
   }
}
#endif

static HRESULT APIENTRY
d3d9_create_resource2(HANDLE handle, D3DDDIARG_CREATERESOURCE2 *arg)
{
   D3D9_HERE();
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = calloc(1, sizeof *resource);
   struct entangled_nine_resource desc;
   BOOL buffer = arg->Flags.VertexBuffer || arg->Flags.IndexBuffer;
   BOOL system = arg->Pool == D3DDDIPOOL_SYSTEMMEM && arg->SurfCount && arg->pSurfList &&
                 arg->pSurfList[0].pSysMem;
   long result = S_OK;

   if (!resource)
      return E_OUTOFMEMORY;
#if !defined(_WIN64)
   if (arg->Flags.RenderTarget) {
      d3d9_dump_runtime_modes();
      d3d9_dump_runtime_frames("a render target's creation");
   }
#endif
   resource->runtime = arg->hResource;
   resource->format = arg->Format;
   resource->flags = arg->Flags;
   resource->surfaces = arg->SurfCount;
   if (arg->SurfCount && arg->pSurfList) {
      resource->width = arg->pSurfList[0].Width;
      resource->height = arg->pSurfList[0].Height;
      resource->depth = arg->pSurfList[0].Depth;
   }
   if (system) {
      resource->sysmem = calloc(arg->SurfCount, sizeof *resource->sysmem);
      if (!resource->sysmem) {
         free(resource);
         return E_OUTOFMEMORY;
      }
      for (UINT i = 0; i < arg->SurfCount; i++) {
         resource->sysmem[i].memory = (void *)arg->pSurfList[i].pSysMem;
         resource->sysmem[i].pitch = arg->pSurfList[i].SysMemPitch;
         resource->sysmem[i].slice_pitch = arg->pSurfList[i].SysMemSlicePitch;
         resource->sysmem[i].width = arg->pSurfList[i].Width;
         resource->sysmem[i].height = arg->pSurfList[i].Height;
         resource->sysmem[i].depth = arg->pSurfList[i].Depth;
      }
   }

   memset(&desc, 0, sizeof desc);
   desc.width = resource->width;
   desc.height = resource->height;
   desc.depth = resource->depth;
   desc.format = (unsigned)arg->Format;
   desc.multisample = (unsigned)arg->MultisampleType;
   desc.quality = arg->MultisampleQuality;
   desc.fvf = arg->Fvf;
   desc.levels = arg->SurfCount;
   if (arg->Flags.RenderTarget)
      desc.usage |= 0x1; /* D3DUSAGE_RENDERTARGET */
   if (arg->Flags.ZBuffer)
      desc.usage |= 0x2; /* D3DUSAGE_DEPTHSTENCIL */
   if (arg->Flags.WriteOnly && buffer)
      desc.usage |= 0x8; /* D3DUSAGE_WRITEONLY */
   if (arg->Flags.Dynamic)
      desc.usage |= 0x200; /* D3DUSAGE_DYNAMIC */
   if (arg->Flags.AutogenMipmap)
      desc.usage |= 0x400; /* D3DUSAGE_AUTOGENMIPMAP */
   if (arg->Flags.VertexBuffer) {
      desc.kind = ENTANGLED_NINE_VERTEX_BUFFER;
   } else if (arg->Flags.IndexBuffer) {
      desc.kind = ENTANGLED_NINE_INDEX_BUFFER;
   } else if (arg->Flags.Volume) {
      desc.kind = ENTANGLED_NINE_VOLUME;
   } else if (arg->Flags.CubeMap) {
      desc.kind = ENTANGLED_NINE_CUBE;
      desc.levels = arg->SurfCount / 6;
   } else if (arg->Flags.Texture) {
      desc.kind = ENTANGLED_NINE_TEXTURE;
   } else {
      desc.kind = ENTANGLED_NINE_SURFACE;
      desc.depth_stencil = arg->Flags.ZBuffer;
      desc.render_target = arg->Flags.RenderTarget;
   }
   resource->kind = desc.kind;
   resource->levels = desc.levels;
   if (arg->Flags.Primary) {
      resource->chain = TRUE;
      device->chain_width = resource->width;
      device->chain_height = resource->height;
      device->chain_format = arg->Format;
   }

   /* In video memory, or a buffer wherever it is (a draw needs one of
    * Nine's): an object of Nine's. System-memory textures and surfaces stay
    * the runtime's memory, read when they are copied from. */
   if (device->nine && (!system || buffer) && arg->SurfCount) {
      resource->nine = entangled_nine_resource_create(device->nine, &desc, &result);
      if (!resource->nine) {
         d3d9_log("resource kind %d format %u %ux%u x%u failed: 0x%08lx", (int)desc.kind,
                  (unsigned)arg->Format, resource->width, resource->height, arg->SurfCount,
                  (unsigned long)result);
         free(resource->sysmem);
         free(resource);
         return (HRESULT)result;
      }
      if (desc.kind == ENTANGLED_NINE_SURFACE && arg->Flags.RenderTarget &&
          (arg->Format == D3DDDIFMT_X8R8G8B8 || arg->Format == D3DDDIFMT_A8R8G8B8)) {
         resource->allocation = d3d9_allocate(device, arg->hResource, resource->width,
                                              resource->height,
                                              arg->Format == D3DDDIFMT_A8R8G8B8);
         if (resource->allocation)
            resource->pixels = malloc((size_t)resource->width * 4 * resource->height);
      }
   }

   arg->hResource = resource;
   d3d9_note("resource %p: format %u, %ux%ux%u, %u surfaces, pool %u, flags 0x%x%s%s%s",
             (void *)resource, (unsigned)arg->Format, resource->width, resource->height,
             resource->depth, arg->SurfCount, (unsigned)arg->Pool, arg->Flags.Value,
             resource->nine ? ", nine" : "", resource->sysmem ? ", sysmem" : "",
             resource->allocation ? ", allocation" : "");
   if (InterlockedIncrement(&said) <= 40)
      d3d9_log("resource %p: format %u, %ux%u, %u surfaces, pool %u, flags 0x%x/0x%x, source %u%s%s%s",
               (void *)resource, (unsigned)arg->Format, resource->width, resource->height,
               arg->SurfCount, (unsigned)arg->Pool, arg->Flags.Value, arg->Flags2.Value,
               arg->VidPnSourceId,
               resource->nine ? ", nine" : "", resource->sysmem ? ", sysmem" : "",
               resource->allocation ? ", allocation" : "");
   return S_OK;
}

/* The runtime opens the screen's shared primary when a device takes the whole
 * screen: the front buffer of its flip chain, which it then draws to and
 * presents in turn with the back buffer. Its size is the chain's, which the
 * back buffer says when it is created - after this. */
static HRESULT APIENTRY
d3d9_open_resource(HANDLE device, D3DDDIARG_OPENRESOURCE *arg)
{
   D3D9_HERE();
   struct d3d9_resource *resource = calloc(1, sizeof *resource);

   (void)device;
   if (!resource)
      return E_OUTOFMEMORY;
   resource->runtime = arg->hResource;
   resource->opened = TRUE;
   resource->chain = TRUE;
   if (arg->NumAllocations && arg->pOpenAllocationInfo)
      resource->allocation = arg->pOpenAllocationInfo[0].hAllocation;
   arg->hResource = resource;
   d3d9_log("resource %p: opened, %u allocations (the first 0x%x), flags 0x%x", (void *)resource,
            arg->NumAllocations, resource->allocation, arg->Flags.Value);
   return S_OK;
}

/* An opened front buffer gets its render target the first time it is used. */
static void
d3d9_realize(struct d3d9_device *device, struct d3d9_resource *resource)
{
   struct entangled_nine_resource desc;
   long result = S_OK;

   if (!resource || !resource->opened || resource->nine || !device->nine ||
       !device->chain_width)
      return;
   memset(&desc, 0, sizeof desc);
   desc.kind = ENTANGLED_NINE_SURFACE;
   desc.width = device->chain_width;
   desc.height = device->chain_height;
   desc.format = (unsigned)device->chain_format;
   desc.levels = 1;
   desc.usage = 0x1; /* D3DUSAGE_RENDERTARGET */
   desc.render_target = 1;
   resource->nine = entangled_nine_resource_create(device->nine, &desc, &result);
   resource->kind = ENTANGLED_NINE_SURFACE;
   resource->levels = 1;
   resource->surfaces = 1;
   resource->format = device->chain_format;
   resource->width = device->chain_width;
   resource->height = device->chain_height;
   if (resource->nine && !resource->pixels)
      resource->pixels = malloc((size_t)resource->width * 4 * resource->height);
   d3d9_log("front buffer %p: %ux%u, 0x%08lx", (void *)resource, resource->width,
            resource->height, (unsigned long)result);
}

static HRESULT APIENTRY
d3d9_destroy_resource(HANDLE handle, HANDLE resource_handle)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = resource_handle;

   if (!resource)
      return S_OK;
   if (device->indices == resource)
      device->indices = NULL;
   d3d9_resource_free(device, resource);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_lock(HANDLE handle, D3DDDIARG_LOCK *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;
   unsigned box[6] = {0, 0, 0, 0, 0, 1}, flags = 0, pitch = 0, slice_pitch = 0;
   BOOL whole = TRUE;
   void *data = NULL;
   long result;

   if (!resource)
      return E_INVALIDARG;
   d3d9_realize(device, resource);
   D3D9_TRACE("lock %p%s sub %u flags 0x%x range %u+%u given %p", (void *)resource,
              resource->sysmem ? " (sysmem)" : "", arg->SubResourceIndex, arg->Flags.Value,
              arg->Range.Offset, arg->Range.Size, arg->pSurfData);
   if (arg->Flags.RangeValid) {
      box[0] = arg->Range.Offset;
      box[2] = arg->Range.Offset + arg->Range.Size;
      whole = FALSE;
   } else if (arg->Flags.AreaValid) {
      box[0] = arg->Area.left;
      box[1] = arg->Area.top;
      box[2] = arg->Area.right;
      box[3] = arg->Area.bottom;
      whole = FALSE;
   } else if (arg->Flags.BoxValid) {
      box[0] = arg->Box.Left;
      box[1] = arg->Box.Top;
      box[2] = arg->Box.Right;
      box[3] = arg->Box.Bottom;
      box[4] = arg->Box.Front;
      box[5] = arg->Box.Back;
      whole = FALSE;
   }
   /* System memory: the runtime has the pointer and only says it will write.
    * Remembered for a buffer, whose copy of Nine's follows at the unlock. */
   if (resource->sysmem || arg->Flags.NotifyOnly) {
      resource->notified_offset = whole ? 0 : box[0];
      resource->notified_size = whole ? resource->width : box[2] - box[0];
      resource->notified_flags = (arg->Flags.NoOverwrite ? 0x1000 : 0) |
                                 (arg->Flags.Discard ? 0x2000 : 0);
      return S_OK;
   }
   if (!device->nine || !resource->nine) {
      static volatile LONG said;

      if (InterlockedIncrement(&said) <= 8)
         d3d9_log("lock of %p (format %u, %ux%u): neither system memory nor Nine's",
               (void *)resource, (unsigned)resource->format, resource->width, resource->height);
      return E_FAIL;
   }
   if (arg->Flags.ReadOnly)
      flags |= 0x10; /* D3DLOCK_READONLY */
   if (arg->Flags.NoOverwrite)
      flags |= 0x1000; /* D3DLOCK_NOOVERWRITE */
   if (arg->Flags.Discard)
      flags |= 0x2000; /* D3DLOCK_DISCARD */
   result = entangled_nine_lock(device->nine, resource->nine, resource->kind, resource->levels,
                                arg->SubResourceIndex, whole ? NULL : box, flags, &data, &pitch,
                                &slice_pitch);
   if (result >= 0) {
      arg->pSurfData = data;
      arg->Pitch = pitch;
      arg->SlicePitch = slice_pitch;
   }
   d3d9_note("  lock of %p.%u: 0x%08lx, %p pitch %u", (void *)resource, arg->SubResourceIndex,
             (unsigned long)result, data, pitch);
   D3D9_CHECK("lock", result);
}

static HRESULT APIENTRY
d3d9_unlock(HANDLE handle, const D3DDDIARG_UNLOCK *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   if (!resource || !device->nine || !resource->nine)
      return S_OK;
   if (resource->sysmem) {
      /* A buffer in system memory: what the runtime wrote, into Nine's. */
      unsigned box[6] = {0, 0, 0, 0, 0, 1}, pitch, slice_pitch;
      UINT offset = min(resource->notified_offset, resource->width);
      UINT size = min(resource->notified_size, resource->width - offset);
      void *data = NULL;
      long result;

      /* Not a dynamic one no draw reads Nine's copy of. */
      if (resource->flags.Dynamic && !resource->copied)
         return S_OK;
      if (!size)
         return S_OK;
      box[0] = offset;
      box[2] = offset + size;
      result = entangled_nine_lock(device->nine, resource->nine, resource->kind, 1, 0, box,
                                   resource->notified_flags, &data, &pitch, &slice_pitch);
      if (result >= 0 && data) {
         memcpy(data, (const char *)resource->sysmem[0].memory + offset, size);
         result = entangled_nine_unlock(device->nine, resource->nine, resource->kind, 1, 0);
      }
      D3D9_CHECK("buffer update", result);
   }
   if (arg->Flags.NotifyOnly)
      return S_OK;
   D3D9_CHECK("unlock", entangled_nine_unlock(device->nine, resource->nine, resource->kind,
                                              resource->levels, arg->SubResourceIndex));
}

/* The lock the runtime takes for a buffer it will not wait for (a dynamic
 * buffer locked with discard or no-overwrite): the same lock here. Nine
 * gives a discarded buffer new storage itself, so the "rename" the runtime
 * follows a discard with has nothing left to do. */
static HRESULT APIENTRY
d3d9_lock_async(HANDLE handle, D3DDDIARG_LOCKASYNC *arg)
{
   D3D9_HERE();
   D3DDDIARG_LOCK lock;
   HRESULT hr;

   memset(&lock, 0, sizeof lock);
   lock.hResource = arg->hResource;
   lock.SubResourceIndex = arg->SubResourceIndex;
   lock.Flags.NoOverwrite = arg->Flags.NoOverwrite;
   lock.Flags.Discard = arg->Flags.Discard;
   lock.Flags.RangeValid = arg->Flags.RangeValid;
   lock.Flags.AreaValid = arg->Flags.AreaValid;
   lock.Flags.BoxValid = arg->Flags.BoxValid;
   lock.Flags.NotifyOnly = arg->Flags.NotifyOnly;
   lock.Box = arg->Box;
   hr = d3d9_lock(handle, &lock);
   arg->pSurfData = lock.pSurfData;
   arg->Pitch = lock.Pitch;
   arg->SlicePitch = lock.SlicePitch;
   return hr;
}

static HRESULT APIENTRY
d3d9_unlock_async(HANDLE handle, const D3DDDIARG_UNLOCKASYNC *arg)
{
   D3D9_HERE();
   D3DDDIARG_UNLOCK unlock;

   memset(&unlock, 0, sizeof unlock);
   unlock.hResource = arg->hResource;
   unlock.SubResourceIndex = arg->SubResourceIndex;
   unlock.Flags.NotifyOnly = arg->Flags.NotifyOnly;
   return d3d9_unlock(handle, &unlock);
}

static HRESULT APIENTRY
d3d9_rename(HANDLE handle, const D3DDDIARG_RENAME *arg)
{
   D3D9_HERE();
   (void)handle;
   (void)arg;
   return S_OK;
}

/* A dimension of a mip level. */
static UINT
d3d9_level(UINT size, UINT level)
{
   size >>= level;
   return size ? size : 1;
}

/* UpdateTexture and UpdateSurface: a texture in system memory onto one in
 * video memory, the rectangle the runtime names and its image in every
 * level below. */
static HRESULT APIENTRY
d3d9_tex_blt1(HANDLE handle, const D3DDDIARG_TEXBLT1 *arg)
{
   D3D9_HERE();
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;
   UINT levels, face = arg->CubeMapFace;
   long result = S_OK;

   if (!device->nine || !dst || !src || !dst->nine)
      return S_OK;
   if (!src->sysmem) {
      if (InterlockedIncrement(&said) <= 3)
         d3d9_log("TexBlt from video memory: not done");
      return S_OK;
   }
   levels = min(max(dst->levels, 1), max(src->levels, 1));
   for (UINT level = 0; level < levels && result >= 0; level++) {
      UINT src_index = src->kind == ENTANGLED_NINE_CUBE ? face * max(src->levels, 1) + level
                                                        : level;
      UINT dst_index = dst->kind == ENTANGLED_NINE_CUBE ? face * max(dst->levels, 1) + level
                                                        : level;
      const struct d3d9_sysmem *from;
      long rect[4];
      UINT x, y;

      if (src_index >= src->surfaces)
         break;
      from = &src->sysmem[src_index];
      /* The rectangle at this level, grown to whole 4x4 blocks so that a
       * compressed format copies whole blocks. */
      rect[0] = (arg->SrcRect.left >> level) & ~3;
      rect[1] = (arg->SrcRect.top >> level) & ~3;
      rect[2] = min((LONG)from->width, (((arg->SrcRect.right - 1) >> level) + 4) & ~3);
      rect[3] = min((LONG)from->height, (((arg->SrcRect.bottom - 1) >> level) + 4) & ~3);
      x = (arg->DstPoint.x >> level) & ~3;
      y = (arg->DstPoint.y >> level) & ~3;
      if (rect[2] <= rect[0] || rect[3] <= rect[1])
         continue;
      if (x + (rect[2] - rect[0]) > d3d9_level(dst->width, level) ||
          y + (rect[3] - rect[1]) > d3d9_level(dst->height, level))
         continue;
      result = entangled_nine_upload(device->nine, dst->nine, dst->kind, dst->levels, dst_index,
                                     x, y, from->memory, from->pitch, rect);
   }
   D3D9_CHECK("texture update", result);
}

/* UpdateTexture for volumes: every level whole (the box the runtime names is
 * not looked at), slice by slice and row by row. */
static HRESULT APIENTRY
d3d9_vol_blt1(HANDLE handle, const D3DDDIARG_VOLUMEBLT1 *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;
   long result = S_OK;

   if (!device->nine || !dst || !src || !dst->nine || !src->sysmem)
      return S_OK;
   for (UINT level = 0; level < min(dst->surfaces, src->surfaces) && result >= 0; level++) {
      const struct d3d9_sysmem *from = &src->sysmem[level];
      unsigned pitch = 0, slice_pitch = 0;
      void *data = NULL;

      if (!from->memory || !from->pitch || !from->slice_pitch)
         continue;
      result = entangled_nine_lock(device->nine, dst->nine, dst->kind, dst->levels, level, NULL,
                                   0, &data, &pitch, &slice_pitch);
      if (result < 0 || !data)
         break;
      if (pitch && slice_pitch) {
         UINT rows = min(from->slice_pitch / from->pitch, slice_pitch / pitch);
         UINT bytes = min(from->pitch, pitch);

         for (UINT z = 0; z < max(from->depth, 1); z++) {
            for (UINT y = 0; y < rows; y++)
               memcpy((char *)data + (size_t)z * slice_pitch + (size_t)y * pitch,
                      (const char *)from->memory + (size_t)z * from->slice_pitch +
                         (size_t)y * from->pitch,
                      bytes);
         }
      }
      result = entangled_nine_unlock(device->nine, dst->nine, dst->kind, dst->levels, level);
   }
   D3D9_CHECK("volume update", result);
}

/* The same for buffers. */
static HRESULT APIENTRY
d3d9_buf_blt1(HANDLE handle, const D3DDDIARG_BUFFERBLT1 *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;
   unsigned box[6] = {0, 0, 0, 0, 0, 1}, pitch, slice_pitch;
   void *data = NULL;
   long result;

   if (!device->nine || !dst || !src || !dst->nine || !src->sysmem)
      return S_OK;
   box[0] = arg->Offset;
   box[2] = arg->Offset + arg->SrcRange.Size;
   result = entangled_nine_lock(device->nine, dst->nine, dst->kind, 1, 0, box, 0, &data, &pitch,
                                &slice_pitch);
   if (result >= 0 && data) {
      memcpy(data, (const char *)src->sysmem[0].memory + arg->SrcRange.Offset,
             arg->SrcRange.Size);
      result = entangled_nine_unlock(device->nine, dst->nine, dst->kind, 1, 0);
   }
   D3D9_CHECK("buffer copy", result);
}

static HRESULT APIENTRY
d3d9_blt(HANDLE handle, const D3DDDIARG_BLT *arg)
{
   D3D9_HERE();
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;

   d3d9_realize(device, dst);
   d3d9_realize(device, src);
   D3D9_FRAME("blt %p.%u (%ld,%ld-%ld,%ld) to %p.%u (%ld,%ld-%ld,%ld) flags 0x%x", (void *)src,
              arg->SrcSubResourceIndex, arg->SrcRect.left, arg->SrcRect.top, arg->SrcRect.right,
              arg->SrcRect.bottom, (void *)dst, arg->DstSubResourceIndex, arg->DstRect.left,
              arg->DstRect.top, arg->DstRect.right, arg->DstRect.bottom, arg->Flags.Value);
   if (!device->nine || !dst || !src)
      return S_OK;
   if (!dst->nine || !src->nine) {
      if (InterlockedIncrement(&said) <= 3)
         d3d9_log("Blt with system memory (%s): not done", src->nine ? "to it" : "from it");
      return S_OK;
   }
   D3D9_CHECK("blt", entangled_nine_blit(device->nine, src->nine, src->kind, src->levels,
                                         arg->SrcSubResourceIndex, (const long *)&arg->SrcRect,
                                         dst->nine, dst->kind, dst->levels,
                                         arg->DstSubResourceIndex, (const long *)&arg->DstRect,
                                         arg->Flags.Linear ? 2 : 1));
}

static HRESULT APIENTRY
d3d9_color_fill(HANDLE handle, const D3DDDIARG_COLORFILL *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   d3d9_realize(device, resource);
   D3D9_FRAME("fill %p.%u (%ld,%ld-%ld,%ld) 0x%08x", (void *)resource, arg->SubResourceIndex,
              arg->DstRect.left, arg->DstRect.top, arg->DstRect.right, arg->DstRect.bottom,
              (unsigned)arg->Color);
   if (!device->nine || !resource || !resource->nine)
      return S_OK;
   D3D9_CHECK("color fill", entangled_nine_fill(device->nine, resource->nine, resource->kind,
                                                resource->levels, arg->SubResourceIndex,
                                                (const long *)&arg->DstRect, arg->Color));
}

static HRESULT APIENTRY
d3d9_generate_mips(HANDLE handle, const D3DDDIARG_GENERATEMIPSUBLEVELS *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   if (!device->nine || !resource || !resource->nine ||
       resource->kind == ENTANGLED_NINE_SURFACE)
      return S_OK;
   return (HRESULT)entangled_nine_generate_mips(device->nine, resource->nine);
}

/* ---------------------------------------------------------------- state */

static HRESULT APIENTRY
d3d9_set_render_target(HANDLE handle, const D3DDDIARG_SETRENDERTARGET *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hRenderTarget;

   d3d9_realize(device, resource);
   D3D9_FRAME("target %u: %p.%u%s", arg->RenderTargetIndex, (void *)resource,
              arg->SubResourceIndex, resource && !resource->nine ? " (not Nine's)" : "");
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("render target",
              entangled_nine_set_target(device->nine, arg->RenderTargetIndex,
                                        resource ? resource->nine : NULL,
                                        resource ? resource->kind : ENTANGLED_NINE_SURFACE,
                                        resource ? resource->levels : 0, arg->SubResourceIndex));
}

static HRESULT APIENTRY
d3d9_set_depth_stencil(HANDLE handle, const D3DDDIARG_SETDEPTHSTENCIL *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hZBuffer;

   D3D9_FRAME("depth: %p%s", (void *)resource,
              resource && !resource->nine ? " (not Nine's)" : "");
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("depth buffer",
              entangled_nine_set_depth(device->nine, resource ? resource->nine : NULL,
                                       resource ? resource->kind : ENTANGLED_NINE_SURFACE,
                                       resource ? resource->levels : 0, 0));
}

static HRESULT APIENTRY
d3d9_clear(HANDLE handle, const D3DDDIARG_CLEAR *arg, UINT count, const RECT *rects)
{
   struct d3d9_device *device = handle;

   D3D9_HERE();
   D3D9_FRAME("clear flags 0x%x colour 0x%08x, %u rects%s", arg->Flags,
              (unsigned)arg->FillColor, count, count ? ":" : "");
   for (UINT i = 0; i < count && i < 4; i++)
      D3D9_FRAME("  (%ld,%ld-%ld,%ld)", rects[i].left, rects[i].top, rects[i].right,
                 rects[i].bottom);
   if (!device->nine)
      return S_OK;
   /* RECT and D3DRECT are the same four LONGs. The low three flags are the
    * API's; the fourth (D3DCLEAR_COMPUTERECTS) is the runtime's own. */
   D3D9_CHECK("clear", entangled_nine_clear(device->nine, count, (const long *)rects,
                                            arg->Flags & 7, arg->FillColor, arg->FillDepth,
                                            arg->FillStencil));
}

static HRESULT APIENTRY
d3d9_set_render_state(HANDLE handle, const D3DDDIARG_RENDERSTATE *arg)
{
   struct d3d9_device *device = handle;

   D3D9_HERE();
   /* Nine refuses the states it does not know; the runtime sends some of its
    * own, and a refusal here would fail the program's call. */
   if (device->nine)
      entangled_nine_render_state(device->nine, (unsigned)arg->State, arg->Value);
   return S_OK;
}

/* The DDI keeps the numbering of Direct3D 7 and 8, where what became sampler
 * states were texture stage states. */
static HRESULT APIENTRY
d3d9_set_texture_stage_state(HANDLE handle, const D3DDDIARG_TEXTURESTAGESTATE *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   unsigned sampler = 0;

   if (!device->nine)
      return S_OK;
   switch ((unsigned)arg->State) {
   case 13: sampler = 1; break;  /* ADDRESSU */
   case 14: sampler = 2; break;  /* ADDRESSV */
   case 25: sampler = 3; break;  /* ADDRESSW */
   case 15: sampler = 4; break;  /* BORDERCOLOR */
   case 16: sampler = 5; break;  /* MAGFILTER */
   case 17: sampler = 6; break;  /* MINFILTER */
   case 18: sampler = 7; break;  /* MIPFILTER */
   case 19: sampler = 8; break;  /* MIPMAPLODBIAS */
   case 20: sampler = 9; break;  /* MAXMIPLEVEL */
   case 21: sampler = 10; break; /* MAXANISOTROPY */
   case 29: sampler = 11; break; /* SRGBTEXTURE */
   case 30: sampler = 12; break; /* ELEMENTINDEX */
   case 31: sampler = 13; break; /* DMAPOFFSET */
   default: break;
   }
   if (sampler)
      entangled_nine_sampler_state(device->nine, arg->Stage, sampler, arg->Value);
   else if ((unsigned)arg->State >= 1 && (unsigned)arg->State <= 32)
      entangled_nine_texture_stage_state(device->nine, arg->Stage, (unsigned)arg->State,
                                         arg->Value);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_texture(HANDLE handle, UINT stage, HANDLE texture)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = texture;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("texture", entangled_nine_texture(device->nine, stage,
                                                resource && resource->kind != ENTANGLED_NINE_SURFACE
                                                   ? resource->nine
                                                   : NULL));
}

static HRESULT APIENTRY
d3d9_set_viewport(HANDLE handle, const D3DDDIARG_VIEWPORTINFO *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   unsigned rect[4] = {arg->X, arg->Y, arg->Width, arg->Height};

   D3D9_TRACE("viewport %u,%u %ux%u", arg->X, arg->Y, arg->Width, arg->Height);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("viewport", entangled_nine_viewport(device->nine, rect, NULL));
}

static HRESULT APIENTRY
d3d9_set_z_range(HANDLE handle, const D3DDDIARG_ZRANGE *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   float range[2] = {arg->MinZ, arg->MaxZ};

   D3D9_TRACE("depth range %g to %g", arg->MinZ, arg->MaxZ);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("depth range", entangled_nine_viewport(device->nine, NULL, range));
}

static HRESULT APIENTRY
d3d9_set_scissor_rect(HANDLE handle, const RECT *rect)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("scissor", entangled_nine_scissor(device->nine, (const long *)rect));
}

static HRESULT APIENTRY
d3d9_set_clip_plane(HANDLE handle, const D3DDDIARG_SETCLIPPLANE *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("clip plane", entangled_nine_clip_plane(device->nine, arg->Index, arg->Plane));
}

static HRESULT APIENTRY
d3d9_set_transform(HANDLE handle, const D3DDDIARG_SETTRANSFORM *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("transform", entangled_nine_transform(device->nine, (unsigned)arg->TransformType,
                                                    (const float *)&arg->Matrix, 0));
}

static HRESULT APIENTRY
d3d9_multiply_transform(HANDLE handle, const D3DDDIARG_MULTIPLYTRANSFORM *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("transform", entangled_nine_transform(device->nine, (unsigned)arg->TransformType,
                                                    (const float *)&arg->Matrix, 1));
}

static HRESULT APIENTRY
d3d9_set_material(HANDLE handle, const D3DDDIARG_SETMATERIAL *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   /* The same seventeen floats as a D3DMATERIAL9. */
   D3D9_CHECK("material", entangled_nine_material(device->nine, (const float *)arg));
}

static HRESULT APIENTRY
d3d9_set_light(HANDLE handle, const D3DDDIARG_SETLIGHT *arg, const D3DDDI_LIGHT *light)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   switch (arg->DataType) {
   case D3DDDI_SETLIGHT_ENABLE:
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, NULL, 1));
   case D3DDDI_SETLIGHT_DISABLE:
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, NULL, 0));
   default:
      /* D3DDDI_LIGHT is laid out as D3DLIGHT9. */
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, light, -1));
   }
}

static HRESULT APIENTRY
d3d9_destroy_light(HANDLE handle, const D3DDDIARG_DESTROYLIGHT *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (device->nine)
      entangled_nine_light(device->nine, arg->Index, NULL, 0);
   return S_OK;
}

/* -------------------------------------------------------------- shaders */

/* Bring-up: with ENTANGLED_D3D9_SHADERS=1, the bytecode of every shader the
 * runtime hands over, eight tokens a line (the runtime makes shaders of its
 * own for the fixed-function pipeline, and they are not in any file). */
static void
d3d9_dump_shader(const char *what, const UINT *code, UINT bytes)
{
   char want[4] = "";
   UINT count = bytes / 4;

   GetEnvironmentVariableA("ENTANGLED_D3D9_SHADERS", want, sizeof want);
   if (want[0] != '1')
      return;
   d3d9_log("%s: %u tokens", what, count);
   for (UINT i = 0; i < count && i < 512; i += 8) {
      char line[160];
      int at = 0;

      for (UINT j = i; j < i + 8 && j < count; j++)
         at += _snprintf(line + at, sizeof line - 1 - at, " %08x", code[j]);
      line[at] = 0;
      d3d9_log(" %s", line);
   }
}

/* A vertex declaration. What a vertex is comes from here alone: the runtime
 * gives a program's XYZRHW vertices the usage POSITIONT itself, and
 * DrawPrimitive2 is only "from the runtime's own buffer", not "transformed". */
struct d3d9_declaration {
   void *nine;
};

static HRESULT APIENTRY
d3d9_create_vertex_shader_decl(HANDLE handle, D3DDDIARG_CREATEVERTEXSHADERDECL *arg,
                               const D3DDDIVERTEXELEMENT *elements)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   declaration = calloc(1, sizeof *declaration);
   if (!declaration)
      return E_OUTOFMEMORY;
   for (UINT i = 0; i < arg->NumVertexElements; i++)
      D3D9_TRACE("declaration element %u: stream %u offset %u type %u method %u usage %u.%u", i,
                 elements[i].Stream, elements[i].Offset, elements[i].Type, elements[i].Method,
                 elements[i].Usage, elements[i].UsageIndex);
   declaration->nine = entangled_nine_declaration_create(device->nine, elements,
                                                         arg->NumVertexElements, &result);
   if (!declaration->nine) {
      free(declaration);
      D3D9_CHECK("vertex declaration", result);
   }
   arg->ShaderHandle = declaration;
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_decl(HANDLE handle, HANDLE declaration_handle)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration = declaration_handle;

   D3D9_TRACE("declaration %p", declaration_handle);
   device->declaration = declaration;
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("vertex declaration",
              entangled_nine_declaration(device->nine, declaration ? declaration->nine : NULL));
}

static HRESULT APIENTRY
d3d9_delete_vertex_shader_decl(HANDLE handle, HANDLE declaration_handle)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration = declaration_handle;

   if (!declaration)
      return S_OK;
   if (device->declaration == declaration)
      device->declaration = NULL;
   entangled_nine_release(declaration->nine);
   free(declaration);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_create_vertex_shader_func(HANDLE handle, D3DDDIARG_CREATEVERTEXSHADERFUNC *arg,
                               const UINT *code)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   d3d9_dump_shader("vertex shader", code, arg->Size);
   arg->ShaderHandle = entangled_nine_vertex_shader_create(device->nine, code, &result);
   D3D9_CHECK("vertex shader", result);
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_func(HANDLE handle, HANDLE shader)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   D3D9_TRACE("vertex shader %p", shader);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("vertex shader", entangled_nine_vertex_shader(device->nine, shader));
}

static HRESULT APIENTRY
d3d9_create_pixel_shader(HANDLE handle, D3DDDIARG_CREATEPIXELSHADER *arg, const UINT *code)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   d3d9_dump_shader("pixel shader", code, arg->CodeSize);
   arg->ShaderHandle = entangled_nine_pixel_shader_create(device->nine, code, &result);
   D3D9_CHECK("pixel shader", result);
}

static HRESULT APIENTRY
d3d9_set_pixel_shader(HANDLE handle, HANDLE shader)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   D3D9_TRACE("pixel shader %p", shader);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("pixel shader", entangled_nine_pixel_shader(device->nine, shader));
}

/* A declaration or a shader going away. */
static HRESULT APIENTRY
d3d9_delete_object(HANDLE handle, HANDLE object)
{
   D3D9_HERE();
   (void)handle;
   entangled_nine_release(object);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONST *arg,
                             const void *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'f', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const_i(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONSTI *arg,
                               const INT *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'i', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const_b(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONSTB *arg,
                               const BOOL *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'b', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONST *arg,
                            const FLOAT *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'f', arg->Register,
                                                          data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const_i(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONSTI *arg,
                              const INT *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'i', arg->Register,
                                                          data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const_b(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONSTB *arg,
                              const BOOL *data)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'b', arg->Register,
                                                          data, arg->Count));
}

/* -------------------------------------------------------------- drawing */

static HRESULT APIENTRY
d3d9_set_stream_source(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCE *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hVertexBuffer;

   D3D9_TRACE("stream %u: buffer %p%s offset %u stride %u", arg->Stream, (void *)resource,
              resource && resource->sysmem ? " (sysmem)" : "", arg->Offset, arg->Stride);
   if (arg->Stream == 0) {
      device->um_vertices = NULL;
      device->stream0_stride = arg->Stride;
      /* A dynamic buffer in system memory is the runtime's own scratch for
       * vertices it processed or was handed (Draw...UP): it writes there
       * without a word, so the vertices are read at the draw, as memory of
       * the program's is. */
      if (resource && resource->sysmem && resource->flags.Dynamic) {
         device->um_vertices = (const char *)resource->sysmem[0].memory + arg->Offset;
         return S_OK;
      }
   }
   if (!device->nine)
      return S_OK;
   if (resource && resource->sysmem && resource->flags.Dynamic && !resource->copied &&
       resource->nine) {
      /* A dynamic buffer as another stream than 0: this draw, and those after
       * it, read Nine's copy, which has been left behind until now. */
      unsigned pitch, slice_pitch;
      void *data = NULL;

      resource->copied = TRUE;
      if (entangled_nine_lock(device->nine, resource->nine, resource->kind, 1, 0, NULL,
                              0x2000 /* D3DLOCK_DISCARD */, &data, &pitch, &slice_pitch) >= 0 &&
          data) {
         memcpy(data, resource->sysmem[0].memory, resource->width);
         entangled_nine_unlock(device->nine, resource->nine, resource->kind, 1, 0);
      }
   }
   D3D9_CHECK("stream", entangled_nine_stream(device->nine, arg->Stream,
                                              resource ? resource->nine : NULL, arg->Offset,
                                              arg->Stride));
}

static HRESULT APIENTRY
d3d9_set_stream_source_um(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCEUM *arg,
                          const void *memory)
{
   D3D9_HERE();
   static volatile LONG said;
   struct d3d9_device *device = handle;

   D3D9_TRACE("stream %u: memory %p stride %u", arg->Stream, memory, arg->Stride);
   /* Nine draws from the program's memory in stream 0 only, which is the
    * only stream the API lets a program do it in. */
   if (arg->Stream == 0) {
      device->um_vertices = memory;
      device->stream0_stride = arg->Stride;
   } else if (InterlockedIncrement(&said) <= 3) {
      d3d9_log("stream %u from memory: not done", arg->Stream);
   }
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_stream_source_freq(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCEFREQ *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("stream frequency", entangled_nine_stream_frequency(device->nine, arg->Stream,
                                                                  arg->Divider));
}

static HRESULT APIENTRY
d3d9_set_indices(HANDLE handle, const D3DDDIARG_SETINDICES *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hIndexBuffer;

   device->um_indices = NULL;
   device->indices = resource;
   /* As for stream 0: the runtime's own scratch for indices. */
   if (resource && resource->sysmem && resource->flags.Dynamic) {
      device->um_indices = resource->sysmem[0].memory;
      device->um_index_size = arg->Stride;
      device->indices = NULL;
      return S_OK;
   }
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("indices", entangled_nine_indices(device->nine, resource ? resource->nine : NULL));
}

static HRESULT APIENTRY
d3d9_set_indices_um(HANDLE handle, UINT index_size, const void *memory)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   device->um_indices = memory;
   device->um_index_size = index_size;
   return S_OK;
}

/* How many vertices `count` primitives of a type are. */
static UINT
d3d9_vertex_count(D3DPRIMITIVETYPE type, UINT count)
{
   switch (type) {
   case D3DPT_POINTLIST: return count;
   case D3DPT_LINELIST: return count * 2;
   case D3DPT_LINESTRIP: return count + 1;
   case D3DPT_TRIANGLELIST: return count * 3;
   default: return count + 2; /* a strip or a fan */
   }
}

/* Indices from memory over vertices in a buffer: Nine draws either all from
 * memory or all from buffers, so the indices go into a buffer of ours. */
static long
d3d9_draw_with_indices(struct d3d9_device *device, D3DPRIMITIVETYPE type, INT base_vertex,
                       UINT min_index, UINT vertices, const void *indices, UINT index_size,
                       UINT count)
{
   UINT bytes = d3d9_vertex_count(type, count) * index_size;
   unsigned box[6] = {0, 0, bytes, 0, 0, 1}, pitch, slice_pitch;
   void *data = NULL;
   long result = S_OK;

   if (!device->scratch_indices || device->scratch_size < bytes ||
       device->um_index_size != index_size) {
      struct entangled_nine_resource desc;

      entangled_nine_release(device->scratch_indices);
      memset(&desc, 0, sizeof desc);
      desc.kind = ENTANGLED_NINE_INDEX_BUFFER;
      desc.width = max(bytes, 64 * 1024);
      desc.format = index_size == 4 ? 102 : 101; /* D3DFMT_INDEX32, D3DFMT_INDEX16 */
      desc.usage = 0x208; /* D3DUSAGE_DYNAMIC | D3DUSAGE_WRITEONLY */
      device->scratch_indices = entangled_nine_resource_create(device->nine, &desc, &result);
      device->scratch_size = device->scratch_indices ? desc.width : 0;
      device->um_index_size = index_size;
      if (!device->scratch_indices)
         return result;
   }
   result = entangled_nine_lock(device->nine, device->scratch_indices,
                                ENTANGLED_NINE_INDEX_BUFFER, 1, 0, box, 0x2000 /* DISCARD */,
                                &data, &pitch, &slice_pitch);
   if (result < 0 || !data)
      return result;
   memcpy(data, indices, bytes);
   entangled_nine_unlock(device->nine, device->scratch_indices, ENTANGLED_NINE_INDEX_BUFFER, 1,
                         0);
   entangled_nine_indices(device->nine, device->scratch_indices);
   result = entangled_nine_draw_indexed(device->nine, type, base_vertex, min_index, vertices, 0,
                                        count);
   entangled_nine_indices(device->nine, device->indices ? device->indices->nine : NULL);
   return result;
}

static HRESULT APIENTRY
d3d9_draw_primitive(HANDLE handle, const D3DDDIARG_DRAWPRIMITIVE *arg, const UINT *flag_buffer)
{
   struct d3d9_device *device = handle;

   (void)flag_buffer;
   InterlockedIncrement(&count_draws);
   D3D9_HERE();
   D3D9_TRACE("draw: type %u start %u count %u%s", (unsigned)arg->PrimitiveType, arg->VStart,
              arg->PrimitiveCount, device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices)
      D3D9_CHECK("draw", entangled_nine_draw_up(device->nine, arg->PrimitiveType,
                                                arg->PrimitiveCount,
                                                (const char *)device->um_vertices +
                                                   (size_t)arg->VStart * device->stream0_stride,
                                                device->stream0_stride));
   D3D9_CHECK("draw", entangled_nine_draw(device->nine, arg->PrimitiveType, arg->VStart,
                                          arg->PrimitiveCount));
}

static HRESULT APIENTRY
d3d9_draw_indexed_primitive(HANDLE handle, const D3DDDIARG_DRAWINDEXEDPRIMITIVE *arg)
{
   struct d3d9_device *device = handle;

   InterlockedIncrement(&count_draws);
   D3D9_HERE();
   if (!device->nine)
      return S_OK;
   if (device->um_vertices && device->um_indices)
      D3D9_CHECK("draw", entangled_nine_draw_indexed_up(
                            device->nine, arg->PrimitiveType, arg->MinIndex, arg->NumVertices,
                            arg->PrimitiveCount,
                            (const char *)device->um_indices +
                               (size_t)arg->StartIndex * device->um_index_size,
                            device->um_index_size,
                            (const char *)device->um_vertices +
                               (ptrdiff_t)arg->BaseVertexIndex * device->stream0_stride,
                            device->stream0_stride));
   if (device->um_indices)
      D3D9_CHECK("draw", d3d9_draw_with_indices(
                            device, arg->PrimitiveType, arg->BaseVertexIndex, arg->MinIndex,
                            arg->NumVertices,
                            (const char *)device->um_indices +
                               (size_t)arg->StartIndex * device->um_index_size,
                            device->um_index_size, arg->PrimitiveCount));
   D3D9_CHECK("draw", entangled_nine_draw_indexed(device->nine, arg->PrimitiveType,
                                                  arg->BaseVertexIndex, arg->MinIndex,
                                                  arg->NumVertices, arg->StartIndex,
                                                  arg->PrimitiveCount));
}

/* The "2" forms: vertices the runtime has put in stream 0 itself (the
 * program's own from a Draw...UP, whatever they are: the declaration says),
 * addressed in bytes. */
static HRESULT APIENTRY
d3d9_draw_primitive2(HANDLE handle, const D3DDDIARG_DRAWPRIMITIVE2 *arg)
{
   struct d3d9_device *device = handle;
   UINT stride = device->stream0_stride;
   long result;

   InterlockedIncrement(&count_draws);
   D3D9_HERE();
   D3D9_TRACE("draw2: type %u offset %u count %u stride %u%s", (unsigned)arg->PrimitiveType,
              arg->FirstVertexOffset, arg->PrimitiveCount, stride,
              device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices) {
      const float *v = (const float *)((const char *)device->um_vertices + arg->FirstVertexOffset);

      D3D9_TRACE("  first vertex at %p: %g %g %g %g 0x%08x", (const void *)v, v[0], v[1], v[2],
                 v[3], ((const unsigned *)v)[4]);
   }
   if (device->um_vertices)
      result = entangled_nine_draw_up(device->nine, arg->PrimitiveType, arg->PrimitiveCount,
                                      (const char *)device->um_vertices + arg->FirstVertexOffset,
                                      stride);
   else
      result = entangled_nine_draw(device->nine, arg->PrimitiveType,
                                   stride ? arg->FirstVertexOffset / stride : 0,
                                   arg->PrimitiveCount);
   D3D9_CHECK("draw", result);
}

static HRESULT APIENTRY
d3d9_draw_indexed_primitive2(HANDLE handle, const D3DDDIARG_DRAWINDEXEDPRIMITIVE2 *arg,
                             UINT index_size, const void *indices, const UINT *flag_buffer)
{
   struct d3d9_device *device = handle;
   UINT stride = device->stream0_stride;
   const void *first = (const char *)indices + arg->StartIndexOffset;
   long result;

   (void)flag_buffer;
   InterlockedIncrement(&count_draws);
   D3D9_HERE();
   D3D9_TRACE("draw indexed2: type %u base %d count %u stride %u%s",
              (unsigned)arg->PrimitiveType, arg->BaseVertexOffset, arg->PrimitiveCount, stride,
              device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices)
      result = entangled_nine_draw_indexed_up(
         device->nine, arg->PrimitiveType, arg->MinIndex, arg->NumVertices, arg->PrimitiveCount,
         first, index_size, (const char *)device->um_vertices + arg->BaseVertexOffset, stride);
   else
      result = d3d9_draw_with_indices(device, arg->PrimitiveType,
                                      stride ? arg->BaseVertexOffset / (INT)stride : 0,
                                      arg->MinIndex, arg->NumVertices, first, index_size,
                                      arg->PrimitiveCount);
   D3D9_CHECK("draw", result);
}

/* -------------------------------------------------------------- queries */

static HRESULT APIENTRY
d3d9_create_query(HANDLE handle, D3DDDIARG_CREATEQUERY *arg)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->hQuery = NULL;
   if (!device->nine)
      return S_OK;
   arg->hQuery = entangled_nine_query_create(device->nine, (unsigned)arg->QueryType, &result);
   D3D9_FRAME("query of type %u: %p", (unsigned)arg->QueryType, (void *)arg->hQuery);
   D3D9_CHECK("query", result);
}

static HRESULT APIENTRY
d3d9_issue_query(HANDLE handle, const D3DDDIARG_ISSUEQUERY *arg)
{
   D3D9_HERE();
   (void)handle;
   D3D9_FRAME("query %p %s", (void *)arg->hQuery, arg->Flags.Begin ? "begins" : "ends");
   if (!arg->hQuery)
      return S_OK;
   /* D3DISSUE_END is 1 and D3DISSUE_BEGIN 2. */
   D3D9_CHECK("query", entangled_nine_query_issue(arg->hQuery, arg->Flags.Begin ? 2 : 1));
}

static HRESULT APIENTRY
d3d9_get_query_data(HANDLE handle, const D3DDDIARG_GETQUERYDATA *arg)
{
   D3D9_HERE();
   long result;

   (void)handle;
   if (!arg->hQuery)
      return S_OK;
   result = entangled_nine_query_data(arg->hQuery, arg->pData,
                                      entangled_nine_query_size(arg->hQuery), 1);
   D3D9_FRAME("query %p data: 0x%08lx", (void *)arg->hQuery, (unsigned long)result);
   return (HRESULT)result;
}

/* -------------------------------------------------------------- present */

static HRESULT APIENTRY
d3d9_flush1(HANDLE handle, UINT flags)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   (void)flags;
   if (device->nine)
      entangled_nine_flush(device->nine);
   return S_OK;
}

/* Microseconds since some fixed moment. */
static LONGLONG
d3d9_now(void)
{
   static LARGE_INTEGER frequency;
   LARGE_INTEGER now;

   if (!frequency.QuadPart)
      QueryPerformanceFrequency(&frequency);
   QueryPerformanceCounter(&now);
   return now.QuadPart * 1000000 / frequency.QuadPart;
}

/* The claim on the host's plane, which the desktop's driver inside DWM reads
 * (d3d10_wddm_plane_claimed): a tick count and this process, in a section
 * both map. Renewed at every present that goes to the plane. */
static void
d3d9_claim_plane(void)
{
   static volatile LONG *claim;
   static BOOL looked;

   if (!looked) {
      SECURITY_DESCRIPTOR descriptor;
      SECURITY_ATTRIBUTES attributes = {sizeof attributes, &descriptor, FALSE};
      HANDLE section;

      InitializeSecurityDescriptor(&descriptor, SECURITY_DESCRIPTOR_REVISION);
      SetSecurityDescriptorDacl(&descriptor, TRUE, NULL, FALSE);
      section = CreateFileMappingA(INVALID_HANDLE_VALUE, &attributes, PAGE_READWRITE, 0, 16,
                                   "Local\\EntangledPlaneClaim");
      if (section)
         claim = MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 16);
      looked = TRUE;
   }
   if (claim) {
      claim[1] = (LONG)GetCurrentProcessId();
      claim[0] = (LONG)GetTickCount();
   }
}

/* Whether a window of this process is in front and covers the whole screen
 * with a client area of the given size: a game "full screen in a window",
 * whose frames can go to the plane as an exclusive one's do. */
static BOOL
d3d9_covers_screen(UINT width, UINT height)
{
   HWND window = GetForegroundWindow();
   POINT origin = {0, 0};
   DWORD process = 0;
   RECT client;

   if (!window || !GetWindowThreadProcessId(window, &process) ||
       process != GetCurrentProcessId() || !GetClientRect(window, &client) ||
       !ClientToScreen(window, &origin))
      return FALSE;
   return origin.x == 0 && origin.y == 0 && (UINT)client.right == width &&
          (UINT)client.bottom == height && (UINT)GetSystemMetrics(SM_CXSCREEN) == width &&
          (UINT)GetSystemMetrics(SM_CYSCREEN) == height;
}

/* Where the frames' time went, since it was last said. */
static struct {
   LONGLONG last, read, escape, present, frames;
   LONG draws;
   unsigned count;
} pace;

/* A present: the frame leaves the GPU for the back buffer's allocation, and
 * the kernel driver copies it on to wherever Windows shows this window (the
 * surface DWM composes from, or the screen). Through the guest's CPU, like
 * every window under this driver so far (ADR-0010); the plane is N4's. */
static HRESULT APIENTRY
d3d9_present1(HANDLE handle, const D3DDDIARG_PRESENT1 *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource;
   struct d3d9_escape_write write;
   D3DDDICB_PRESENT present;
   long result;
   HRESULT hr;

   InterlockedIncrement(&count_presents);
   D3D9_HERE();
   D3D9_FRAME("present");
   if (!device->nine || !arg->SrcResources || !arg->phSrcResources)
      return S_OK;
   resource = arg->phSrcResources[0].hResource;
   d3d9_realize(device, resource);
   if (resource && resource->nine &&
       (resource->chain || d3d9_covers_screen(resource->width, resource->height))) {
      /* The whole screen, exclusively or as a window in front that covers it:
       * the frame onto the host's plane, copied on the GPU.
       * The kernel driver is told (a lease of two seconds, renewed here) so
       * that it leaves the primary under the plane alone. Nothing is flipped:
       * what the screen shows is the plane. */
      LONGLONG t0 = d3d9_now(), t1;

      if (entangled_nine_show(device->nine, resource->nine, resource->width,
                              resource->height)) {
         DWORD now = GetTickCount();

         d3d9_claim_plane();
         device->on_plane = TRUE;
         if (now - device->leased >= 500) {
            struct {
               unsigned short type;
               unsigned short length;
               char text[236];
            } lease;

            device->leased = now;
            memset(&lease, 0, sizeof lease);
            lease.type = 0x901;
            d3d9_escape(device, &lease, sizeof lease);
         }
         t1 = d3d9_now();
         pace.read += t1 - t0;
         if (pace.last)
            pace.frames += t1 - pace.last;
         pace.last = t1;
         if (++pace.count == 120) {
            LONG draws = count_draws;

            d3d9_log("pace: %u.%u ms a frame (%u.%u fps) on the plane, of which showing it "
                     "%u.%u; %ld draws a frame",
                     (unsigned)(pace.frames / 120000), (unsigned)(pace.frames / 12000 % 10),
                     pace.frames ? (unsigned)(120000000 / pace.frames) : 0,
                     pace.frames ? (unsigned)(1200000000 / pace.frames % 10) : 0,
                     (unsigned)(pace.read / 120000), (unsigned)(pace.read / 12000 % 10),
                     (long)((draws - pace.draws) / 120));
            pace.draws = draws;
            pace.read = pace.escape = pace.present = pace.frames = 0;
            pace.count = 0;
         }
         if (InterlockedIncrement(&said) <= 2)
            d3d9_log("present %ux%u on the plane", resource->width, resource->height);
         return S_OK;
      }
      if (InterlockedIncrement(&said) <= 4)
         d3d9_log("present: the plane is not to be had; through the CPU");
   } else if (device->on_plane && device->nine) {
      /* No longer in front, or no longer the size of the screen: the plane
       * back to the desktop, and this frame to DWM. */
      entangled_nine_show(device->nine, NULL, 0, 0);
      device->on_plane = FALSE;
   }
   if (!resource || !resource->nine || !resource->allocation || !resource->pixels) {
      if (InterlockedIncrement(&said) <= 4)
         d3d9_log("present of %p: nothing to present from", (void *)resource);
      return S_OK;
   }

   LONGLONG t0 = d3d9_now(), t1, t2, t3;

   result = entangled_nine_read_surface(device->nine, resource->nine, resource->pixels,
                                        resource->width, resource->height);
   t1 = d3d9_now();
   if (result < 0) {
      if (InterlockedIncrement(&said) <= 4)
         d3d9_log("present: reading the frame failed 0x%08lx", (unsigned long)result);
      return (HRESULT)result;
   }
   memset(&write, 0, sizeof write);
   write.type = 0x904;
   write.length = 20;
   write.handle = resource->allocation;
   write.buffer = (unsigned long long)(uintptr_t)resource->pixels;
   write.size = (unsigned long long)resource->width * 4 * resource->height;
   {
      NTSTATUS status = d3d9_escape(device, &write, sizeof write);

      if (status < 0 && InterlockedIncrement(&said) <= 4)
         d3d9_log("present: handing the frame to the kernel driver failed 0x%08lx",
                  (unsigned long)status);
   }

   t2 = d3d9_now();
   memset(&present, 0, sizeof present);
   present.hSrcAllocation = resource->allocation;
   present.hContext = device->context;
   hr = device->callbacks.pfnPresentCb(device->runtime, &present);
   t3 = d3d9_now();
   pace.read += t1 - t0;
   pace.escape += t2 - t1;
   pace.present += t3 - t2;
   if (pace.last)
      pace.frames += t3 - pace.last;
   pace.last = t3;
   if (++pace.count == 120) {
      LONG draws = count_draws;

      d3d9_log("pace: %u.%u ms a frame (%u.%u fps), of which reading back %u.%u, to the kernel "
               "%u.%u, its present %u.%u; %ld draws a frame",
               (unsigned)(pace.frames / 120000), (unsigned)(pace.frames / 12000 % 10),
               pace.frames ? (unsigned)(120000000 / pace.frames) : 0,
               pace.frames ? (unsigned)(1200000000 / pace.frames % 10) : 0,
               (unsigned)(pace.read / 120000), (unsigned)(pace.read / 12000 % 10),
               (unsigned)(pace.escape / 120000), (unsigned)(pace.escape / 12000 % 10),
               (unsigned)(pace.present / 120000), (unsigned)(pace.present / 12000 % 10),
               (long)((draws - pace.draws) / 120));
      pace.draws = draws;
      pace.read = pace.escape = pace.present = pace.frames = 0;
      pace.count = 0;
   }
   if (InterlockedIncrement(&said) <= 2 || FAILED(hr))
      d3d9_log("present %ux%u: 0x%08lx", resource->width, resource->height, (unsigned long)hr);
   return hr;
}

/* ------------------------------------------------------- device itself */

static HRESULT APIENTRY
d3d9_destroy_device(HANDLE handle)
{
   D3D9_HERE();
   struct d3d9_device *device = handle;

   if (device->on_plane && device->nine)
      entangled_nine_show(device->nine, NULL, 0, 0);
   entangled_nine_release(device->scratch_indices);
   entangled_nine_close(device->nine);
   if (device->context) {
      D3DDDICB_DESTROYCONTEXT destroy;

      destroy.hContext = device->context;
      device->callbacks.pfnDestroyContextCb(device->runtime, &destroy);
   }
   d3d9_log("device %p destroyed", handle);
   free(device);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_create_device(HANDLE adapter, D3DDDIARG_CREATEDEVICE *arg)
{
   struct d3d9_device *device = calloc(1, sizeof *device);
   D3DDDI_DEVICEFUNCS *funcs = arg->pDeviceFuncs;

#if !defined(_WIN64)
   d3d9_arm_traps();
#endif

   if (!device)
      return E_OUTOFMEMORY;
   device->runtime = arg->hDevice;
   device->adapter = adapter;
   device->callbacks = *arg->pCallbacks;
   d3d9_fill_stubs(funcs);
   funcs->pfnDestroyDevice = d3d9_destroy_device;
   funcs->pfnCreateResource2 = d3d9_create_resource2;
   funcs->pfnOpenResource = d3d9_open_resource;
   funcs->pfnDestroyResource = d3d9_destroy_resource;
   funcs->pfnLock = d3d9_lock;
   funcs->pfnUnlock = d3d9_unlock;
   funcs->pfnLockAsync = d3d9_lock_async;
   funcs->pfnUnlockAsync = d3d9_unlock_async;
   funcs->pfnRename = d3d9_rename;
   funcs->pfnTexBlt1 = d3d9_tex_blt1;
   funcs->pfnBufBlt1 = d3d9_buf_blt1;
   funcs->pfnVolBlt1 = d3d9_vol_blt1;
   funcs->pfnBlt = d3d9_blt;
   funcs->pfnColorFill = d3d9_color_fill;
   funcs->pfnGenerateMipSubLevels = d3d9_generate_mips;
   funcs->pfnSetRenderTarget = d3d9_set_render_target;
   funcs->pfnSetDepthStencil = d3d9_set_depth_stencil;
   funcs->pfnClear = d3d9_clear;
   funcs->pfnSetRenderState = d3d9_set_render_state;
   funcs->pfnSetTextureStageState = d3d9_set_texture_stage_state;
   funcs->pfnSetTexture = d3d9_set_texture;
   funcs->pfnSetViewport = d3d9_set_viewport;
   funcs->pfnSetZRange = d3d9_set_z_range;
   funcs->pfnSetScissorRect = d3d9_set_scissor_rect;
   funcs->pfnSetClipPlane = d3d9_set_clip_plane;
   funcs->pfnSetTransform = d3d9_set_transform;
   funcs->pfnMultiplyTransform = d3d9_multiply_transform;
   funcs->pfnSetMaterial = d3d9_set_material;
   funcs->pfnSetLight = d3d9_set_light;
   funcs->pfnDestroyLight = d3d9_destroy_light;
   funcs->pfnCreateVertexShaderDecl = d3d9_create_vertex_shader_decl;
   funcs->pfnSetVertexShaderDecl = d3d9_set_vertex_shader_decl;
   funcs->pfnDeleteVertexShaderDecl = d3d9_delete_vertex_shader_decl;
   funcs->pfnCreateVertexShaderFunc = d3d9_create_vertex_shader_func;
   funcs->pfnSetVertexShaderFunc = d3d9_set_vertex_shader_func;
   funcs->pfnDeleteVertexShaderFunc = d3d9_delete_object;
   funcs->pfnCreatePixelShader = d3d9_create_pixel_shader;
   funcs->pfnSetPixelShader = d3d9_set_pixel_shader;
   funcs->pfnDeletePixelShader = d3d9_delete_object;
   funcs->pfnSetVertexShaderConst = d3d9_set_vertex_shader_const;
   funcs->pfnSetVertexShaderConstI = d3d9_set_vertex_shader_const_i;
   funcs->pfnSetVertexShaderConstB = d3d9_set_vertex_shader_const_b;
   funcs->pfnSetPixelShaderConst = d3d9_set_pixel_shader_const;
   funcs->pfnSetPixelShaderConstI = d3d9_set_pixel_shader_const_i;
   funcs->pfnSetPixelShaderConstB = d3d9_set_pixel_shader_const_b;
   funcs->pfnSetStreamSource = d3d9_set_stream_source;
   funcs->pfnSetStreamSourceUm = d3d9_set_stream_source_um;
   funcs->pfnSetStreamSourceFreq = d3d9_set_stream_source_freq;
   funcs->pfnSetIndices = d3d9_set_indices;
   funcs->pfnSetIndicesUm = d3d9_set_indices_um;
   funcs->pfnDrawPrimitive = d3d9_draw_primitive;
   funcs->pfnDrawIndexedPrimitive = d3d9_draw_indexed_primitive;
   funcs->pfnDrawPrimitive2 = d3d9_draw_primitive2;
   funcs->pfnDrawIndexedPrimitive2 = d3d9_draw_indexed_primitive2;
   funcs->pfnCreateQuery = d3d9_create_query;
   funcs->pfnDestroyQuery = d3d9_delete_object;
   funcs->pfnIssueQuery = d3d9_issue_query;
   funcs->pfnGetQueryData = d3d9_get_query_data;
   funcs->pfnFlush1 = d3d9_flush1;
   funcs->pfnPresent1 = d3d9_present1;
   /* A driver of this interface version has the newer form of six functions
    * and must leave the older one NULL: the runtime fails CreateDevice
    * otherwise ("UMD did not provide non-null pfnPresent1, and null
    * pfnPresent", and so on down the list). */
   funcs->pfnPresent = NULL;
   funcs->pfnFlush = NULL;
   funcs->pfnCreateResource = NULL;
   funcs->pfnVolBlt = NULL;
   funcs->pfnTexBlt = NULL;
   funcs->pfnBufBlt = NULL;
   /* No asynchronous locks: with these three given, Windows 10's runtime
    * plays the device's calls from a worker thread of its own and calls
    * these from the program's thread at the same time, and nothing under
    * this front (Nine, the threaded context, Zink) may be entered by two
    * threads. Without them every call comes in order from one thread at a
    * time. ENTANGLED_D3D9_ASYNC=1 gives them back, to work on that. */
   {
      char want[4] = "";

      GetEnvironmentVariableA("ENTANGLED_D3D9_ASYNC", want, sizeof want);
      if (want[0] != '1') {
         funcs->pfnLockAsync = NULL;
         funcs->pfnUnlockAsync = NULL;
         funcs->pfnRename = NULL;
      }
   }
   arg->hDevice = device;
   d3d9_log("device %p: interface %u, version 0x%x, flags 0x%x", (void *)device,
            arg->Interface, arg->Version, arg->Flags.Value);
   d3d9_sample_this_thread();
   {
      D3DDDICB_CREATECONTEXT create;
      HRESULT hr;

      memset(&create, 0, sizeof create);
      hr = device->callbacks.pfnCreateContextCb(device->runtime, &create);
      if (SUCCEEDED(hr))
         device->context = create.hContext;
      else
         d3d9_log("no kernel context (0x%08lx): nothing will be presented", (unsigned long)hr);
   }
   {
      struct entangled_nine_info info;
      long result;

      device->nine = entangled_nine_open(&info, &result);
      if (device->nine)
         d3d9_log("nine: %s, vs 0x%x, ps 0x%x, textures to %u, %u render targets",
                  info.renderer, info.vertex_shader, info.pixel_shader, info.max_texture_width,
                  info.simultaneous_rts);
      else
         d3d9_log("nine: no device (0x%08lx)", (unsigned long)result);
   }
   return S_OK;
}

/* -------------------------------------------------------------- adapter */

static HRESULT APIENTRY
d3d9_close_adapter(HANDLE handle)
{
   D3D9_HERE();
   d3d9_log("adapter %p closed", handle);
   free(handle);
   return S_OK;
}

HRESULT APIENTRY
OpenAdapter(D3DDDIARG_OPENADAPTER *arg)
{
   struct d3d9_adapter *adapter;
   char exe[MAX_PATH] = "?";
   char want[8] = "";
   const char *base = exe;

   /* Every program is served. ENTANGLED_D3D9=0 in a program's environment
    * tells it there is no driver, which is what it was told before this DLL
    * existed (and what sends a game to its own Vulkan or software path). */
   GetModuleFileNameA(NULL, exe, sizeof exe);
   for (const char *p = exe; *p; p++) {
      if (*p == '\\' || *p == '/')
         base = p + 1;
   }
   GetEnvironmentVariableA("ENTANGLED_D3D9", want, sizeof want);
   if (strcmp(want, "0") == 0) {
      d3d9_log("not serving %.150s: ENTANGLED_D3D9=0", base);
      return E_FAIL;
   }

   adapter = calloc(1, sizeof *adapter);
   if (!adapter)
      return E_OUTOFMEMORY;
   adapter->runtime = arg->hAdapter;
   adapter->callbacks = *arg->pAdapterCallbacks;
   arg->hAdapter = adapter;
   arg->pAdapterFuncs->pfnGetCaps = d3d9_get_caps;
   arg->pAdapterFuncs->pfnCreateDevice = d3d9_create_device;
   arg->pAdapterFuncs->pfnCloseAdapter = d3d9_close_adapter;
   arg->DriverVersion = D3D_UMD_INTERFACE_VERSION;

   d3d9_log("adapter %p in %.150s (%u-bit): interface %u, version 0x%x", (void *)adapter,
            exe, (unsigned)(8 * sizeof(void *)), arg->Interface, arg->Version);
   d3d9_log("answering driver version 0x%x", arg->DriverVersion);
   return S_OK;
}

BOOL WINAPI
DllMain(HINSTANCE instance, DWORD reason, void *reserved)
{
   (void)reserved;
   if (reason == DLL_PROCESS_ATTACH) {
      DisableThreadLibraryCalls(instance);
      InitializeCriticalSection(&log_lock);
      log_ready = TRUE;
      crash_module = instance;
      crash_handler = AddVectoredExceptionHandler(0, d3d9_crash);
   } else if (reason == DLL_PROCESS_DETACH) {
      /* A handler left behind would be a jump into a DLL that is gone. */
      if (crash_handler)
         RemoveVectoredExceptionHandler(crash_handler);
   }
   return TRUE;
}
