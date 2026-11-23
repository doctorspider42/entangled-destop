/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3dkmthk.h>

#include <cstdio>
#include <cstdlib>
#include <cwchar>

static void fail(const char *operation, unsigned long status)
{
    std::fprintf(stderr, "WDDM FENCE FAIL: %s (0x%08lX)\n", operation, status);
    std::exit(1);
}

static void require_success(NTSTATUS status, const char *operation)
{
    if (status < 0)
        fail(operation, static_cast<unsigned long>(status));
}

static D3DKMT_HANDLE monitored_fence(D3DKMT_HANDLE device)
{
    D3DKMT_CREATESYNCHRONIZATIONOBJECT2 create = {};
    create.hDevice = device;
    create.Info.Type = D3DDDI_MONITORED_FENCE;
    require_success(D3DKMTCreateSynchronizationObject2(&create), "create monitored fence");
    return create.hSyncObject;
}

static void destroy_fence(D3DKMT_HANDLE fence)
{
    const D3DKMT_DESTROYSYNCHRONIZATIONOBJECT destroy = {fence};
    require_success(D3DKMTDestroySynchronizationObject(&destroy), "destroy monitored fence");
}

void verify_wddm_fence(LUID luid)
{
    D3DKMT_OPENADAPTERFROMLUID adapter = {};
    adapter.AdapterLuid = luid;
    require_success(D3DKMTOpenAdapterFromLuid(&adapter), "open adapter");

    D3DKMT_CREATEDEVICE device = {};
    device.hAdapter = adapter.hAdapter;
    require_success(D3DKMTCreateDevice(&device), "create WDDM device");

    D3DKMT_CREATECONTEXTVIRTUAL context = {};
    context.hDevice = device.hDevice;
    context.Flags.SynchronizationOnly = 1;
    context.ClientHint = D3DKMT_CLIENTHINT_DX12;
    require_success(D3DKMTCreateContextVirtual(&context), "create virtual D3D12 context");

    const D3DKMT_HANDLE gate = monitored_fence(device.hDevice);
    const D3DKMT_HANDLE complete = monitored_fence(device.hDevice);
    constexpr UINT64 target = 1;
    const HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event)
        fail("create completion event", GetLastError());

    D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMCPU event_wait = {};
    event_wait.hDevice = device.hDevice;
    event_wait.ObjectCount = 1;
    event_wait.ObjectHandleArray = &complete;
    event_wait.FenceValueArray = &target;
    event_wait.hAsyncEvent = event;
    require_success(D3DKMTWaitForSynchronizationObjectFromCpu(&event_wait), "register CPU completion wait");

    D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMGPU queue_wait = {};
    queue_wait.hContext = context.hContext;
    queue_wait.ObjectCount = 1;
    queue_wait.ObjectHandleArray = &gate;
    queue_wait.MonitoredFenceValueArray = &target;
    require_success(D3DKMTWaitForSynchronizationObjectFromGpu(&queue_wait), "queue wait for gate");

    D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMGPU queue_signal = {};
    queue_signal.hContext = context.hContext;
    queue_signal.ObjectCount = 1;
    queue_signal.ObjectHandleArray = &complete;
    queue_signal.MonitoredFenceValueArray = &target;
    require_success(D3DKMTSignalSynchronizationObjectFromGpu(&queue_signal), "queue completion signal");

    const DWORD before = WaitForSingleObject(event, 150);
    if (before == WAIT_FAILED)
        fail("wait before releasing gate", GetLastError());
    if (before != WAIT_TIMEOUT)
        fail("completion overtook the queued wait", before);

    D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMCPU release = {};
    release.hDevice = device.hDevice;
    release.ObjectCount = 1;
    release.ObjectHandleArray = &gate;
    release.FenceValueArray = &target;
    require_success(D3DKMTSignalSynchronizationObjectFromCpu(&release), "signal gate from CPU");

    const DWORD after = WaitForSingleObject(event, 10000);
    if (after == WAIT_FAILED)
        fail("wait after releasing gate", GetLastError());
    if (after != WAIT_OBJECT_0)
        fail("completion failed after releasing gate", after);
    CloseHandle(event);

    destroy_fence(complete);
    destroy_fence(gate);
    const D3DKMT_DESTROYCONTEXT destroy_context = {context.hContext};
    require_success(D3DKMTDestroyContext(&destroy_context), "destroy virtual context");
    const D3DKMT_DESTROYDEVICE destroy_device = {device.hDevice};
    require_success(D3DKMTDestroyDevice(&destroy_device), "destroy WDDM device");
    const D3DKMT_CLOSEADAPTER close_adapter = {adapter.hAdapter};
    require_success(D3DKMTCloseAdapter(&close_adapter), "close adapter");

    std::wprintf(L"WDDM FENCE PASS: queued completion observed only after gate release\n");
}
