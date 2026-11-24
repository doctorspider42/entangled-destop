// SPDX-License-Identifier: Apache-2.0
// Export Dwm-Core and DxgKrnl event headers from a QPC-clock ETL file.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <evntrace.h>
#include <evntcons.h>

#include <cstdio>

namespace {
constexpr GUID dwm_core{0x9e9bba3c, 0x2e38, 0x40cb, {0x99, 0xf4, 0x9e, 0x82, 0x81, 0x42, 0x51, 0x64}};
constexpr GUID dxg_krnl{0x802ec45a, 0x1e99, 0x4b83, {0x99, 0x20, 0x87, 0xc9, 0x82, 0x77, 0xba, 0x9d}};

void WINAPI record(EVENT_RECORD *event)
{
    const auto &header = event->EventHeader;
    const char *provider = header.ProviderId == dwm_core ? "dwm" : header.ProviderId == dxg_krnl ? "dxg" : nullptr;
    if (!provider)
        return;

    printf("%lld,%s,%u,%u,%u,%lu,%lu\n", header.TimeStamp.QuadPart, provider,
           static_cast<unsigned int>(header.EventDescriptor.Id),
           static_cast<unsigned int>(header.EventDescriptor.Task),
           static_cast<unsigned int>(header.EventDescriptor.Opcode), header.ProcessId, header.ThreadId);
}
} // namespace

int wmain(int argc, wchar_t **argv)
{
    if (argc != 2) {
        fprintf(stderr, "usage: etw-display-qpc.exe trace.etl\n");
        return 2;
    }

    EVENT_TRACE_LOGFILEW log{};
    log.LogFileName = argv[1];
    log.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    log.EventRecordCallback = record;
    TRACEHANDLE trace = OpenTraceW(&log);
    if (trace == INVALID_PROCESSTRACE_HANDLE) {
        fprintf(stderr, "OpenTraceW failed (%lu)\n", GetLastError());
        return 3;
    }
    fprintf(stderr, "qpc_frequency=%lld events_lost=%lu buffers_lost=%lu\n",
            log.LogfileHeader.PerfFreq.QuadPart, log.LogfileHeader.EventsLost, log.LogfileHeader.BuffersLost);
    puts("qpc,provider,event,task,op,pid,tid");
    const ULONG status = ProcessTrace(&trace, 1, nullptr, nullptr);
    CloseTrace(trace);
    if (status != ERROR_SUCCESS) {
        fprintf(stderr, "ProcessTrace failed (%lu)\n", status);
        return 4;
    }
    return fflush(stdout) == 0 && !ferror(stdout) ? 0 : 5;
}
