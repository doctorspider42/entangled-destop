// Check 5: every invocation writes f(i) into element i of one storage buffer.
// f must match `checks::compute_f` in src/checks.rs bit for bit — it is pure
// u32 arithmetic (wrapping multiply, xor, shift, add), so there is no rounding
// for two implementations to disagree about.

@group(0) @binding(0) var<storage, read_write> data: array<u32>;

fn f(i: u32) -> u32 {
    var x = i * 2654435761u;
    x = x ^ (x >> 15u);
    return x + (i << 3u) + 0x9e3779b9u;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i < arrayLength(&data)) {
        data[i] = f(i);
    }
}
