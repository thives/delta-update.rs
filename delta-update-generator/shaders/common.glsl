// Buffers, parameters, random numbers, byte access and size arithmetic shared by every stage.
// Include after `#version 450` and `layout.glsl`.

// A stage that gives one thread to each candidate sets LOCAL_SIZE to 1 before including this file,
// so every candidate is a work group of its own. Candidates take different paths and different
// times, and threads of one work group run in lockstep, so a work group of candidates would run
// them one after another. Separate work groups run side by side.
#ifndef LOCAL_SIZE
#define LOCAL_SIZE WORKGROUP
#endif
layout(local_size_x = LOCAL_SIZE) in;

// The immutable old and new images as little-endian words, the old image first and the new image
// from word `P_NEW_BASE`. The host pads each with whole zero words past its end, so a word load that
// straddles the last byte stays inside the buffer.
layout(std430, set = 0, binding = 0) readonly buffer Images {
    uint img[];
};
// Parameters, control, population, scratch, replay and scan workspaces, and every chromosome.
layout(std430, set = 0, binding = 1) buffer State {
    uint st[];
};
// The sorted copy-source tables and the per-cell classification.
layout(std430, set = 0, binding = 2) buffer Index {
    uint ix[];
};
// The replay's runs, one vector each. Two arrays per candidate.
layout(std430, set = 0, binding = 3) buffer Runs {
    uvec4 rv[];
};

// Per-dispatch values. `gen` is the generation, the rest are stage-specific.
layout(push_constant) uniform Push {
    uint gen;
    uint a0;
    uint a1;
    uint a2;
    uint a3;
} pc;

uint P(uint k) {
    return st[ST_PARAMS + k];
}

// ---- Bytes ---------------------------------------------------------------------------------
// The word at which the new image starts. Every stage that reads an image calls `images_init`
// first, which reads it once.
uint NB;

void images_init() {
    NB = st[ST_PARAMS + P_NEW_BASE];
}

uint new_base() {
    return NB;
}

uint old_byte(uint i) {
    return (img[i >> 2u] >> ((i & 3u) << 3u)) & 0xffu;
}

uint new_byte(uint i) {
    return (img[new_base() + (i >> 2u)] >> ((i & 3u) << 3u)) & 0xffu;
}

// Four bytes from byte offset `i`, in little-endian order. The caller keeps `i + 4` inside the
// image or accepts the zero padding.
uint img_u32(uint base, uint i) {
    uint w = base + (i >> 2u);
    uint s = (i & 3u) << 3u;
    if (s == 0u) {
        return img[w];
    }
    return (img[w] >> s) | (img[w + 1u] << (32u - s));
}

uint old_u32(uint i) {
    return img_u32(0u, i);
}

uint new_u32(uint i) {
    return img_u32(new_base(), i);
}

// ---- Random numbers ------------------------------------------------------------------------
// A PCG step. A stream is a pure function of its four seed words, so a candidate's choices do not
// depend on how the work was batched or scheduled.
uint rng_next(inout uint s) {
    s = s * 747796405u + 2891336453u;
    uint w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

uint rng_mix(uint v) {
    uint s = v * 747796405u + 2891336453u;
    uint w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

uint rng_seed(uint a, uint b, uint c, uint d) {
    uint s = rng_mix(a ^ 0x9E3779B9u);
    s = rng_mix(s ^ b);
    s = rng_mix(s + c * 0x85EBCA6Bu);
    s = rng_mix(s ^ (d * 0xC2B2AE35u));
    return s;
}

// A value in [0, n). `n` must be positive.
uint rng_below(inout uint s, uint n) {
    return rng_next(s) % n;
}

bool rng_percent(inout uint s, uint percent) {
    return rng_next(s) % 100u < percent;
}

// A length in [1, max], log-uniform, so short and long lengths are both explored.
uint rng_log_length(inout uint s, uint max_len) {
    if (max_len <= 1u) {
        return 1u;
    }
    uint bits = uint(findMSB(max_len)) + 1u;
    uint shift = rng_below(s, bits);
    uint len = 1u << shift;
    uint jitter = rng_next(s) % len;
    return min(len + jitter, max_len);
}

// ---- Protobuf sizes ------------------------------------------------------------------------
// The encoded length of a varint.
uint vlen(uint v) {
    if (v < 128u) return 1u;
    if (v < 16384u) return 2u;
    if (v < 2097152u) return 3u;
    if (v < 268435456u) return 4u;
    return 5u;
}

// acc += v, refusing a result past SUM_LIMIT. `acc` is unchanged on refusal.
bool sum_add(inout uint acc, uint v) {
    if (v > uint(SUM_LIMIT) - acc) {
        return false;
    }
    acc += v;
    return true;
}

// ---- Control -------------------------------------------------------------------------------
void ctl_add(uint word, uint v) {
    atomicAdd(st[ST_CTL + word], v);
}

uint slot_base(uint id) {
    return uint(ST_SLOTS) + id * uint(SLOT_WORDS);
}

// The index in `rv` of the first run of candidate `j`'s first run array.
uint runs_base(uint j) {
    return j * uint(RUN_SETS * RUN_CAP);
}

uint scan_base(uint j) {
    return uint(ST_SCAN) + j * uint(SCAN_MAX * SCANW);
}
