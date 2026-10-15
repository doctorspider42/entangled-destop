// Native feasibility probe: Metal and an ARM64 guest access the same pages.
// This is not a Linux boot or a Venus compatibility test.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <Hypervisor/Hypervisor.h>
#include <sys/mman.h>
#include <unistd.h>

static bool hv_ok(hv_return_t result, const char *operation) {
    if (result == HV_SUCCESS) return true;
    fprintf(stderr, "%s failed: %#x\n", operation, (unsigned)result);
    return false;
}

static bool metal_marker(void *pages, size_t length) {
    @autoreleasepool {
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        id<MTLBuffer> buffer = nil;
        NSError *error = nil;
        id<MTLLibrary> library = nil;
        id<MTLComputePipelineState> pipeline = nil;
        id<MTLCommandQueue> queue = nil;
        id<MTLCommandBuffer> command = nil;
        id<MTLComputeCommandEncoder> encoder = nil;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        if (!device) { fprintf(stderr, "No Metal device\n"); return false; }
        printf("Metal device: %s\n", device.name.UTF8String);
        // The nested autorelease pool releases all Metal references before munmap.
        buffer = [device newBufferWithBytesNoCopy:pages length:length
                      options:MTLResourceStorageModeShared deallocator:nil];
        if (!buffer) { fprintf(stderr, "Metal refused shared pages\n"); return false; }
        library = [device newLibraryWithSource:
            @"#include <metal_stdlib>\nusing namespace metal;\n"
             "kernel void write_marker(device uint *out [[buffer(0)]]) { out[0] = 41; }"
            options:nil error:&error];
        pipeline = library ?
            [device newComputePipelineStateWithFunction:[library newFunctionWithName:@"write_marker"]
                                                 error:&error] : nil;
        if (!pipeline) { fprintf(stderr, "Metal shader: %s\n", error.description.UTF8String); return false; }
        queue = [device newCommandQueue];
        command = [queue commandBuffer];
        encoder = [command computeCommandEncoder];
        if (!encoder) { fprintf(stderr, "Metal command allocation failed\n"); return false; }
        [encoder setComputePipelineState:pipeline];
        [encoder setBuffer:buffer offset:0 atIndex:0];
        [encoder dispatchThreads:MTLSizeMake(1, 1, 1) threadsPerThreadgroup:MTLSizeMake(1, 1, 1)];
        [encoder endEncoding];
        {
            [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
                (void)completed;
                dispatch_semaphore_signal(done);
            }];
        }
        [command commit];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC))) {
            // The GPU might still use these pages. Process teardown owns cleanup.
            fprintf(stderr, "Metal command timed out\n");
            _exit(1);
        }
        if (command.status != MTLCommandBufferStatusCompleted || *(uint32_t *)pages != 41) {
            fprintf(stderr, "Metal marker failed: %s\n", command.error.description.UTF8String);
            return false;
        }
        return true;
    }
}

int main(void) {
    @autoreleasepool {
        int status = 1;
        bool vm_created = false, mapped = false, cpu_created = false;
        hv_vcpu_t cpu = 0;
        hv_vcpu_exit_t *exit = NULL;
        size_t length = (size_t)getpagesize();
        void *pages = mmap(NULL, length, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANON, -1, 0);
        if (pages == MAP_FAILED) { perror("mmap"); return 1; }
        if (!metal_marker(pages, length)) goto cleanup;
        if (!hv_ok(hv_vm_create(NULL), "hv_vm_create")) goto cleanup;
        vm_created = true;
        if (!hv_ok(hv_vm_map(pages, 0x40000000, length,
                            HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC), "hv_vm_map")) goto cleanup;
        mapped = true;
        // ldr w0,[x1]; add w0,w0,#1; hvc #0. Code and marker occupy different bytes.
        const uint32_t code[] = {0xb9400020, 0x11000400, 0xd4000002};
        memcpy((char *)pages + 0x100, code, sizeof(code));
        if (!hv_ok(hv_vcpu_create(&cpu, &exit, NULL), "hv_vcpu_create")) goto cleanup;
        cpu_created = true;
        if (!hv_ok(hv_vcpu_set_reg(cpu, HV_REG_PC, 0x40000100), "set PC") ||
            !hv_ok(hv_vcpu_set_reg(cpu, HV_REG_X1, 0x40000000), "set X1") ||
            !hv_ok(hv_vcpu_set_reg(cpu, HV_REG_CPSR, 0x3c5), "set CPSR") ||
            !hv_ok(hv_vcpu_run(cpu), "hv_vcpu_run")) goto cleanup;
        uint64_t marker = 0;
        if (!hv_ok(hv_vcpu_get_reg(cpu, HV_REG_X0, &marker), "get X0")) goto cleanup;
        if (exit->reason != HV_EXIT_REASON_EXCEPTION ||
            (exit->exception.syndrome >> 26) != 0x16 || marker != 42) {
            fprintf(stderr, "Unexpected guest exit: reason=%u syndrome=%#llx marker=%llu\n",
                    exit->reason, exit->exception.syndrome, marker);
            goto cleanup;
        }
        puts("PASS: Metal wrote 41; ARM64 guest read the same allocation and returned 42 through HVC");
        status = 0;
    cleanup:
        // A failed teardown must not free RAM the hypervisor might still use.
        if (cpu_created && !hv_ok(hv_vcpu_destroy(cpu), "hv_vcpu_destroy")) _exit(1);
        if (mapped && !hv_ok(hv_vm_unmap(0x40000000, length), "hv_vm_unmap")) _exit(1);
        if (vm_created && !hv_ok(hv_vm_destroy(), "hv_vm_destroy")) _exit(1);
        if (munmap(pages, length)) { perror("munmap"); status = 1; }
        return status;
    }
}
