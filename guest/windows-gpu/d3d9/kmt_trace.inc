/*
 * Bring-up only (ENTANGLED_D3D9_TRY bit 16, 64-bit): what d3d9.dll asks the
 * kernel, and what it is answered. The runtime decides whether an adapter
 * has a "HAL" from more than the driver's caps, and says nothing about why
 * not; its D3DKMT calls are the rest of the conversation. They are imports
 * of d3d9.dll, so this DLL, once loaded into the process, can stand in
 * their way: every D3DKMT import is pointed at a wrapper that calls the
 * real function and logs its name and status.
 *
 * Every D3DKMT function takes one pointer, which is what lets one wrapper
 * shape serve them all (and why this is not done on x86, where a wrong
 * argument count would unbalance the stack).
 */
#if defined(_WIN64)

typedef NTSTATUS (APIENTRY *kmt_function)(void *);

static struct {
   const char *name;
   kmt_function original;
} kmt_slots[64];
static unsigned kmt_count;
static volatile LONG kmt_lines;

static NTSTATUS
kmt_call(unsigned slot, void *arg)
{
   NTSTATUS status = kmt_slots[slot].original(arg);

   if (InterlockedIncrement(&kmt_lines) < 600) {
      /* D3DKMT_QUERYADAPTERINFO: hAdapter, Type, pPrivateDriverData, size. */
      if (strcmp(kmt_slots[slot].name, "D3DKMTQueryAdapterInfo") == 0 && arg)
         d3d9_log("kmt %s type %u, %u bytes: 0x%lx", kmt_slots[slot].name,
                  ((UINT *)arg)[1], ((UINT *)arg)[4], (unsigned long)status);
      else
         d3d9_log("kmt %s: 0x%lx", kmt_slots[slot].name, (unsigned long)status);
   }
   return status;
}

#define KMT_W(i) \
   static NTSTATUS APIENTRY kmt_w##i(void *arg) { return kmt_call(i, arg); }
#define KMT_W8(i) \
   KMT_W(i##0) KMT_W(i##1) KMT_W(i##2) KMT_W(i##3) KMT_W(i##4) KMT_W(i##5) KMT_W(i##6) KMT_W(i##7)
KMT_W8(0) KMT_W8(1) KMT_W8(2) KMT_W8(3) KMT_W8(4) KMT_W8(5) KMT_W8(6) KMT_W8(7)
#define KMT_N8(i) \
   kmt_w##i##0, kmt_w##i##1, kmt_w##i##2, kmt_w##i##3, kmt_w##i##4, kmt_w##i##5, kmt_w##i##6, kmt_w##i##7
static const kmt_function kmt_wrappers[64] = {
   KMT_N8(0), KMT_N8(1), KMT_N8(2), KMT_N8(3), KMT_N8(4), KMT_N8(5), KMT_N8(6), KMT_N8(7),
};

static void
kmt_trace_install(void)
{
   static LONG installed;
   BYTE *base = (BYTE *)GetModuleHandleA("d3d9.dll");
   IMAGE_NT_HEADERS *nt;
   IMAGE_DATA_DIRECTORY *dir;
   IMAGE_IMPORT_DESCRIPTOR *import;
   HMODULE self;

   if (!base || InterlockedExchange(&installed, 1) != 0)
      return;
   /* The wrappers live here: this DLL must not leave before d3d9.dll does. */
   GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                      (LPCSTR)kmt_trace_install, &self);

   nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   dir = &nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
   if (!dir->VirtualAddress)
      return;
   for (import = (IMAGE_IMPORT_DESCRIPTOR *)(base + dir->VirtualAddress); import->Name;
        import++) {
      IMAGE_THUNK_DATA *names = (IMAGE_THUNK_DATA *)(base + (import->OriginalFirstThunk
                                                               ? import->OriginalFirstThunk
                                                               : import->FirstThunk));
      IMAGE_THUNK_DATA *slots = (IMAGE_THUNK_DATA *)(base + import->FirstThunk);

      for (; names->u1.AddressOfData; names++, slots++) {
         const char *name;
         DWORD old;

         if (IMAGE_SNAP_BY_ORDINAL(names->u1.Ordinal))
            continue;
         name = ((IMAGE_IMPORT_BY_NAME *)(base + names->u1.AddressOfData))->Name;
         if (strncmp(name, "D3DKMT", 6) != 0 || kmt_count >= 64)
            continue;
         kmt_slots[kmt_count].name = name;
         kmt_slots[kmt_count].original = (kmt_function)slots->u1.Function;
         if (VirtualProtect(&slots->u1.Function, sizeof slots->u1.Function, PAGE_READWRITE,
                            &old)) {
            slots->u1.Function = (ULONG_PTR)kmt_wrappers[kmt_count];
            VirtualProtect(&slots->u1.Function, sizeof slots->u1.Function, old, &old);
            kmt_count++;
         }
      }
   }
   d3d9_log("kmt trace: %u imports of d3d9.dll", kmt_count);
}

#else

static void
kmt_trace_install(void)
{
}

#endif
