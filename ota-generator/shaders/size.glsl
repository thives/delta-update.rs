// The exact uncompressed payload size of a program, and the admission checks that need no
// compression: the encoded manifest's size, and the client's modeled decode memory.
//
// Every number follows the wire form the mapper emits for a raw program. An operation is
// `type | data_offset | data_length | src extents | dst extents | hash`, all present even when
// zero. An extent is two varints in a length-prefixed message. A replacement and a copy each carry
// one 32-byte hash, a zero operation none. The sizes that do not depend on the program (names,
// partition records, signature slots, block size) arrive from the host as `P_PART_FIXED` and
// `P_MAN_FIXED`, computed with the real encoder, so nothing here assumes them.

// The slots a vector of `n` elements holds once grown by pushes: a power of two, at least four.
uint vec_slots(uint n) {
    if (n == 0u) {
        return 0u;
    }
    uint pow2 = n <= 1u ? 1u : (1u << (uint(findMSB(n - 1u)) + 1u));
    return max(4u, pow2);
}

uint dec_slot;
uint dec_heap;
uint dec_trans;

// Charge one vector of `n` elements of `elem` bytes, as `Usage::vector` does.
void dec_vector(uint n, uint elem) {
    uint slots = vec_slots(n);
    if (slots == 0u) {
        return;
    }
    dec_slot += slots * elem;
    dec_heap += 24u;
    if (slots > 4u) {
        dec_trans = max(dec_trans, (slots / 2u) * elem + 24u);
    }
}

// The size of the payload, or the reason it cannot be. `blob` is the data bytes the program
// carries, repair included.
uint raw_size(uint sb, out uint size, out uint blob) {
    uint nops = p_nops(sb);
    uint cursor = 0u;
    uint ops_bytes = 0u;
    uint extents = 0u;
    dec_slot = P(P_DEC_SLOT);
    dec_heap = P(P_DEC_HEAP);
    dec_trans = P(P_DEC_TRANS);
    dec_vector(nops, 192u);
    size = 0u;
    blob = 0u;
    for (uint i = 0u; i < nops; i++) {
        uint kind = o_kind(sb, i);
        uint df = o_dfirst(sb, i);
        uint dn = o_dn(sb, i);
        uint sf = o_sfirst(sb, i);
        uint sn = o_sn(sb, i);
        uint data_len = 0u;
        if (kind == uint(K_REPLACE)) {
            data_len = o_stream(sb, i);
        }
        uint type_len = kind == uint(K_TARGET) ? 2u : 1u;
        uint op_len = 1u + type_len + 1u + vlen(cursor) + 1u + vlen(data_len);
        for (uint e = 0u; e < sn; e++) {
            op_len += 4u + vlen(x_start(sb, sf + e)) + vlen(x_len(sb, sf + e));
        }
        for (uint e = 0u; e < dn; e++) {
            op_len += 4u + vlen(x_start(sb, df + e)) + vlen(x_len(sb, df + e));
        }
        if (kind != uint(K_ZERO)) {
            op_len += 34u;
            dec_heap += 56u;
            dec_trans = max(dec_trans, 56u);
        }
        ops_bytes += 1u + vlen(op_len) + op_len;
        if (!sum_add(cursor, data_len)) {
            return uint(S_OVERFLOW);
        }
        dec_vector(sn, 32u);
        dec_vector(dn, 32u);
        extents += sn + dn;
    }
    uint part_len = P(P_PART_FIXED) + ops_bytes;
    uint man_len = P(P_MAN_FIXED) + 1u + vlen(cursor) + 1u + vlen(part_len) + part_len;
    blob = cursor;
    if (man_len > P(P_MAX_MANIFEST)) {
        return uint(S_ADMIT);
    }
    // The contract workspace, which validation derives from the extents after decoding. The CPU charges
    // it as `Usage::temporary(extents, SPAN_COST)`: 16 bytes per extent and the allocator's 24, and
    // nothing at all when there are no extents. It is a transient, so it counts only when it is the largest.
    if (extents > 0u) {
        if (extents > (uint(SUM_LIMIT) - 24u) / 16u) {
            return uint(S_ADMIT);
        }
        dec_trans = max(dec_trans, extents * 16u + 24u);
    }
    // The decoded total, summed without wrapping. A sum past SUM_LIMIT is refused as past the budget.
    uint dec_total = 0u;
    if (!sum_add(dec_total, dec_slot) || !sum_add(dec_total, dec_heap) || !sum_add(dec_total, dec_trans)) {
        return uint(S_ADMIT);
    }
    if (dec_total > P(P_MAX_DECODED)) {
        return uint(S_ADMIT);
    }
    uint total = uint(0);
    total = P(P_HEADER) + man_len;
    if (!sum_add(total, P(P_SIG2)) || !sum_add(total, cursor)) {
        return uint(S_OVERFLOW);
    }
    size = total;
    return uint(S_OK);
}

// ---- Fingerprints --------------------------------------------------------------------------
// Two 32-bit lanes of a word hash, so equal programs are recognised and different ones almost
// never collide.
uint fp_a;
uint fp_b;

void fp_reset() {
    fp_a = 2166136261u;
    fp_b = 0x9E3779B1u;
}

void fp_word(uint w) {
    fp_a = (fp_a ^ w) * 16777619u;
    fp_a ^= fp_a >> 15u;
    fp_b = (fp_b + w) * 0x85EBCA6Bu;
    fp_b ^= fp_b >> 13u;
}
