// A chromosome in a slot: readers, and a writer that builds a program by appending.
//
// Edits never shift data in place. A move reads a parent slot and writes a child slot front to
// back through the writer, so a child is always packed: its operations in order, and each
// operation's extents contiguous. The writer checks every capacity before it writes. When one is
// exhausted it sets `w_fail` and stops writing, so nothing is truncated silently and nothing
// outside the slot is touched.

uint p_nops(uint sb) {
    return st[sb + H_NOPS];
}

uint p_next(uint sb) {
    return st[sb + H_NEXT];
}

uint o_kind(uint sb, uint i) {
    return st[sb + uint(SLOT_OPS) + i * uint(OPW)];
}

uint o_dfirst(uint sb, uint i) {
    return st[sb + uint(SLOT_OPS) + i * uint(OPW) + 1u];
}

uint o_dn(uint sb, uint i) {
    return st[sb + uint(SLOT_OPS) + i * uint(OPW) + 2u];
}

uint o_sfirst(uint sb, uint i) {
    return st[sb + uint(SLOT_OPS) + i * uint(OPW) + 3u];
}

uint o_sn(uint sb, uint i) {
    return st[sb + uint(SLOT_OPS) + i * uint(OPW) + 4u];
}

uint x_start(uint sb, uint e) {
    return st[sb + uint(SLOT_EXT) + e * 2u];
}

uint x_len(uint sb, uint e) {
    return st[sb + uint(SLOT_EXT) + e * 2u + 1u];
}

bool k_is_copy(uint kind) {
    return kind == uint(K_SOURCE) || kind == uint(K_TARGET);
}

// The destination stream length of operation `i`. Programs read here were evaluated, so their
// sums are within SUM_LIMIT.
uint o_stream(uint sb, uint i) {
    uint first = o_dfirst(sb, i);
    uint n = o_dn(sb, i);
    uint sum = 0u;
    for (uint e = 0u; e < n; e++) {
        sum += x_len(sb, first + e);
    }
    return sum;
}

// Mark a slot as holding nothing: no operations, not evaluated.
void p_clear(uint sb) {
    st[sb + H_NOPS] = 0u;
    st[sb + H_NEXT] = 0u;
    st[sb + H_FIT] = FIT_INVALID;
    st[sb + H_STATUS] = uint(S_SKIP);
    st[sb + H_MOVE] = 0u;
}

// Copy the used part of slot `from` over slot `to`, header included.
void p_copy(uint from, uint to) {
    for (uint h = 0u; h < uint(SLOT_HDR); h++) {
        st[to + h] = st[from + h];
    }
    uint nops = st[from + H_NOPS];
    uint next = st[from + H_NEXT];
    for (uint w = 0u; w < nops * uint(OPW); w++) {
        st[to + uint(SLOT_OPS) + w] = st[from + uint(SLOT_OPS) + w];
    }
    for (uint w = 0u; w < next * 2u; w++) {
        st[to + uint(SLOT_EXT) + w] = st[from + uint(SLOT_EXT) + w];
    }
}

// ---- Writer --------------------------------------------------------------------------------
uint w_sb;
uint w_nops;
uint w_next;
// The operation being written: its kind, whether one is open, and where its extents start. Its
// counts live in registers and reach the slot when the operation closes, so appending an extent
// costs two writes and no read.
uint w_open_flag;
uint w_kind;
uint w_df;
uint w_dn;
uint w_sf;
uint w_sn;
// Set to 1 when a capacity ran out. A flag word, not a bool: some compute drivers mishandle boolean
// globals.
uint w_fail;
// Which capacity ran out, as a status code, when `w_fail` is set.
uint w_why;

void w_begin(uint sb) {
    w_sb = sb;
    w_nops = 0u;
    w_next = 0u;
    w_open_flag = 0u;
    w_dn = 0u;
    w_sn = 0u;
    w_fail = 0u;
    w_why = uint(S_OK);
}

// Write the open operation's record.
void w_close() {
    if (w_open_flag == 0u) {
        return;
    }
    uint at = w_sb + uint(SLOT_OPS) + (w_nops - 1u) * uint(OPW);
    st[at] = w_kind;
    st[at + 1u] = w_df;
    st[at + 2u] = w_dn;
    st[at + 3u] = w_sf;
    st[at + 4u] = w_sn;
    w_open_flag = 0u;
}

// Seal the program: the counts go to the header, and the evaluation state is cleared.
void w_end() {
    w_close();
    st[w_sb + H_NOPS] = w_nops;
    st[w_sb + H_NEXT] = w_next;
    st[w_sb + H_FIT] = FIT_INVALID;
    st[w_sb + H_STATUS] = w_fail != 0u ? w_why : uint(S_OK);
}

void w_open(uint kind) {
    if (w_fail != 0u) {
        return;
    }
    w_close();
    if (w_nops >= uint(OP_CAP)) {
        w_fail = 1u;
        w_why = uint(S_OPCAP);
        return;
    }
    w_nops++;
    w_open_flag = 1u;
    w_kind = kind;
    w_df = w_next;
    w_dn = 0u;
    w_sf = w_next;
    w_sn = 0u;
}

void w_dst(uint start, uint len) {
    if (w_fail != 0u) {
        return;
    }
    if (w_next >= uint(EXT_CAP)) {
        w_fail = 1u;
        w_why = uint(S_EXTCAP);
        return;
    }
    uint at = w_sb + uint(SLOT_EXT) + w_next * 2u;
    st[at] = start;
    st[at + 1u] = len;
    w_next++;
    w_dn++;
}

// Source extents follow the operation's destination extents, never precede them.
void w_src(uint start, uint len) {
    if (w_fail != 0u) {
        return;
    }
    if (w_next >= uint(EXT_CAP)) {
        w_fail = 1u;
        w_why = uint(S_EXTCAP);
        return;
    }
    if (w_sn == 0u) {
        w_sf = w_next;
    }
    uint at = w_sb + uint(SLOT_EXT) + w_next * 2u;
    st[at] = start;
    st[at + 1u] = len;
    w_next++;
    w_sn++;
}

// Like `w_dst` and `w_src`, but an extent that continues the previous one of its list extends it.
// Two contiguous extents are one extent as far as the stream goes, so this changes no byte the
// operation writes or reads, and it saves an extent in the manifest.
void w_dst_join(uint start, uint len) {
    if (w_fail == 0u && w_dn > 0u && w_sn == 0u) {
        uint at = w_sb + uint(SLOT_EXT) + (w_next - 1u) * 2u;
        uint prev_start = st[at];
        uint prev_len = st[at + 1u];
        if (prev_start + prev_len == start && len <= uint(SUM_LIMIT) - prev_len) {
            st[at + 1u] = prev_len + len;
            return;
        }
    }
    w_dst(start, len);
}

void w_src_join(uint start, uint len) {
    if (w_fail == 0u && w_sn > 0u) {
        uint at = w_sb + uint(SLOT_EXT) + (w_next - 1u) * 2u;
        uint prev_start = st[at];
        uint prev_len = st[at + 1u];
        if (prev_start + prev_len == start && len <= uint(SUM_LIMIT) - prev_len) {
            st[at + 1u] = prev_len + len;
            return;
        }
    }
    w_src(start, len);
}

// Append the stream range [from, to) of `n` extents at `first` in `par` as destination extents.
void w_dst_slice(uint par, uint first, uint n, uint from, uint to) {
    uint base = 0u;
    for (uint e = 0u; e < n; e++) {
        if (base >= to) {
            break;
        }
        uint len = x_len(par, first + e);
        uint end = base + len;
        if (end > from) {
            uint a = max(from, base);
            uint b = min(to, end);
            w_dst(x_start(par, first + e) + (a - base), b - a);
        }
        base = end;
    }
}

void w_src_slice(uint par, uint first, uint n, uint from, uint to) {
    uint base = 0u;
    for (uint e = 0u; e < n; e++) {
        if (base >= to) {
            break;
        }
        uint len = x_len(par, first + e);
        uint end = base + len;
        if (end > from) {
            uint a = max(from, base);
            uint b = min(to, end);
            w_src(x_start(par, first + e) + (a - base), b - a);
        }
        base = end;
    }
}

// Append operation `i` of `par` unchanged.
void w_copy_op(uint par, uint i) {
    w_open(o_kind(par, i));
    uint dn = o_dn(par, i);
    uint df = o_dfirst(par, i);
    for (uint e = 0u; e < dn; e++) {
        w_dst(x_start(par, df + e), x_len(par, df + e));
    }
    uint sn = o_sn(par, i);
    uint sf = o_sfirst(par, i);
    for (uint e = 0u; e < sn; e++) {
        w_src(x_start(par, sf + e), x_len(par, sf + e));
    }
}

// Append the stream range [from, to) of operation `i` as an operation of the same kind. A copy's
// source stream is cut at the same positions, so both sides stay equal in length. Nothing is
// written when the range is empty.
void w_part(uint par, uint i, uint from, uint to) {
    if (from >= to) {
        return;
    }
    w_open(o_kind(par, i));
    w_dst_slice(par, o_dfirst(par, i), o_dn(par, i), from, to);
    if (k_is_copy(o_kind(par, i))) {
        w_src_slice(par, o_sfirst(par, i), o_sn(par, i), from, to);
    }
}

// Append a one-extent operation, with a one-extent source for a copy.
void w_simple(uint kind, uint dst_start, uint len, uint src_start) {
    w_open(kind);
    w_dst(dst_start, len);
    if (k_is_copy(kind)) {
        w_src(src_start, len);
    }
}

// ---- Search over a program -----------------------------------------------------------------
// The operation whose destination holds byte `pos`: its index, the stream position of `pos`
// inside it, and the end of the extent that holds it. Any kind is found, so a copy can be placed
// over zeros or another copy, which is an intermediate write the replay allows.
bool find_op_at(uint par, uint pos, out uint op, out uint at, out uint extent_end) {
    uint nops = p_nops(par);
    for (uint i = 0u; i < nops; i++) {
        uint first = o_dfirst(par, i);
        uint n = o_dn(par, i);
        uint before = 0u;
        for (uint e = 0u; e < n; e++) {
            uint s = x_start(par, first + e);
            uint l = x_len(par, first + e);
            if (pos >= s && pos - s < l) {
                op = i;
                at = before + (pos - s);
                extent_end = s + l;
                return true;
            }
            before += l;
        }
    }
    op = 0u;
    at = 0u;
    extent_end = 0u;
    return false;
}

// Replace the stream range [a, a + n) of operation `i` by one operation `mid_kind`, keeping the
// rest of the operation on either side. The result is written through the writer, so the caller
// has begun it. Every other operation is copied. For a copy being cut, the source stream is
// split at the same positions.
void carve(uint par, uint i, uint a, uint n, uint mid_kind, uint mid_dst, uint mid_src) {
    uint nops = p_nops(par);
    uint total = o_stream(par, i);
    for (uint k = 0u; k < i; k++) {
        w_copy_op(par, k);
    }
    w_part(par, i, 0u, a);
    w_simple(mid_kind, mid_dst, n, mid_src);
    w_part(par, i, a + n, total);
    for (uint k = i + 1u; k < nops; k++) {
        w_copy_op(par, k);
    }
}
