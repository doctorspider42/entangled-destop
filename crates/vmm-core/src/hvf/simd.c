// Apple's vector-by-value ABI is not a stable Rust FFI type. Keep it in C.
#include <Hypervisor/Hypervisor.h>
#include <string.h>
int32_t entangled_hvf_get_simd(uint64_t cpu, uint32_t reg, uint8_t *out) {
    hv_simd_fp_uchar16_t value;
    hv_return_t result = hv_vcpu_get_simd_fp_reg(cpu, reg, &value);
    if (result == HV_SUCCESS) memcpy(out, &value, 16);
    return result;
}
int32_t entangled_hvf_set_simd(uint64_t cpu, uint32_t reg, const uint8_t *in) {
    hv_simd_fp_uchar16_t value;
    memcpy(&value, in, 16);
    return hv_vcpu_set_simd_fp_reg(cpu, reg, value);
}
