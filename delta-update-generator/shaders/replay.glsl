// Replay of a program on the target, as runs of bytes with known origins.
//
// This is the memory model of `memory.rs` in a bounded, flat arena. The runs of candidate `j`
// live in the workspace at `rb`, sorted by start and covering [0, new_len) exactly once. A run is
// {start, length, origin kind, origin offset}: its bytes are the old image's, the new image's, or
// zeros, one byte after another from the origin offset. A write splits the runs it overlaps,
// replaces them, and merges neighbours that continue each other, so the target costs memory in
// proportion to its fragmentation and not to the image. Storage past the old image is unknown,
// not zero: an old-origin byte whose offset is past the old image never matches anything.

uint rb;
uint rn;
// Work this replay has spent, against REPLAY_BUDGET, and whether it ran out.
uint rwork;
uint rover;
// The base of the candidate's first run array. A bulk write merges into the other array and swaps,
// and the replay ends with its runs back here.
uint rb_home;

void r_attach(uint base) {
    rb = base;
    rb_home = base;
}

uint r_start(uint r) {
    return rv[rb + r].x;
}

uint r_len(uint r) {
    return rv[rb + r].y;
}

uint r_kind(uint r) {
    return rv[rb + r].z;
}

uint r_off(uint r) {
    return rv[rb + r].w;
}

void r_set(uint r, uint s, uint l, uint k, uint o) {
    rv[rb + r] = uvec4(s, l, k, o);
}

// Move the `count` runs from `first` up by `d` places. The move is made from the top, eight runs at a
// time, loading all eight before storing any, so the loads are in flight together instead of each
// waiting on the one before. A chunk's stores never reach a lower chunk's sources, so the move is
// right when the ranges overlap.
void r_shift_up(uint first, uint count, uint d) {
    uint top = first + count;
    while (top - first >= 8u) {
        uint b = rb + top - 8u;
        uvec4 v0 = rv[b];
        uvec4 v1 = rv[b + 1u];
        uvec4 v2 = rv[b + 2u];
        uvec4 v3 = rv[b + 3u];
        uvec4 v4 = rv[b + 4u];
        uvec4 v5 = rv[b + 5u];
        uvec4 v6 = rv[b + 6u];
        uvec4 v7 = rv[b + 7u];
        rv[b + d] = v0;
        rv[b + 1u + d] = v1;
        rv[b + 2u + d] = v2;
        rv[b + 3u + d] = v3;
        rv[b + 4u + d] = v4;
        rv[b + 5u + d] = v5;
        rv[b + 6u + d] = v6;
        rv[b + 7u + d] = v7;
        top -= 8u;
    }
    while (top > first) {
        top--;
        rv[rb + top + d] = rv[rb + top];
    }
}

// Move the `count` runs from `first` down by `d` places, from the bottom, eight at a time.
void r_shift_down(uint first, uint count, uint d) {
    uint at = first;
    uint end = first + count;
    while (end - at >= 8u) {
        uint b = rb + at;
        uvec4 v0 = rv[b];
        uvec4 v1 = rv[b + 1u];
        uvec4 v2 = rv[b + 2u];
        uvec4 v3 = rv[b + 3u];
        uvec4 v4 = rv[b + 4u];
        uvec4 v5 = rv[b + 5u];
        uvec4 v6 = rv[b + 6u];
        uvec4 v7 = rv[b + 7u];
        uint to = b - d;
        rv[to] = v0;
        rv[to + 1u] = v1;
        rv[to + 2u] = v2;
        rv[to + 3u] = v3;
        rv[to + 4u] = v4;
        rv[to + 5u] = v5;
        rv[to + 6u] = v6;
        rv[to + 7u] = v7;
        at += 8u;
    }
    while (at < end) {
        rv[rb + at - d] = rv[rb + at];
        at++;
    }
}

// The run holding byte `p`: the last run that starts at or before it. Needs `rn > 0` and `p` in
// the image.
uint r_find(uint p) {
    uint lo = 0u;
    uint hi = rn;
    while (hi - lo > 1u) {
        uint mid = (lo + hi) >> 1u;
        if (r_start(mid) <= p) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    return lo;
}

// Two adjacent runs that are one run: the same origin kind, and the second continues the first.
bool r_mergeable(uint a, uint b) {
    uint k = r_kind(a);
    if (k != r_kind(b)) {
        return false;
    }
    return k == uint(O_ZERO) || r_off(a) + r_len(a) == r_off(b);
}

// Start the target as the old image. Beyond the old image its bytes are unknown.
void r_reset(uint new_len) {
    rwork = 0u;
    rover = 0u;
    rn = 0u;
    if (new_len > 0u) {
        r_set(0u, 0u, new_len, uint(O_OLD), 0u);
        rn = 1u;
    }
}

// Write [s, s + n) with bytes from origin (k, off), where `off` is the origin of byte `s`. The
// last write to any byte wins. Returns false, with the runs unchanged, when the arena is full.
bool r_write(uint s, uint n, uint k, uint off) {
    if (rwork > uint(REPLAY_BUDGET)) {
        rover = 1u;
        return false;
    }
    uint e = s + n;
    uint i = r_find(s);
    uint j = r_find(e - 1u);
    uint istart = r_start(i);
    bool hasl = istart < s;
    uint jend = r_start(j) + r_len(j);
    bool hasr = jend > e;
    uint lk = r_kind(i);
    uint loff = r_off(i);
    uint rk = r_kind(j);
    uint roff = rk == uint(O_ZERO) ? 0u : r_off(j) + (e - r_start(j));
    uint removed = j - i + 1u;
    uint added = 1u + (hasl ? 1u : 0u) + (hasr ? 1u : 0u);
    uint nrn = rn - removed + added;
    if (nrn > uint(RUN_CAP)) {
        return false;
    }
    // The tail that moves is the cost of the write.
    rwork += 4u + (added == removed ? 0u : rn - (j + 1u));
    if (added > removed) {
        r_shift_up(j + 1u, rn - (j + 1u), added - removed);
    } else if (added < removed) {
        r_shift_down(j + 1u, rn - (j + 1u), removed - added);
    }
    uint at = i;
    if (hasl) {
        r_set(at, istart, s - istart, lk, loff);
        at++;
    }
    r_set(at, s, n, k, off);
    at++;
    if (hasr) {
        r_set(at, e, jend - e, rk, roff);
        at++;
    }
    rn = nrn;
    // Merge the new pieces with their neighbours.
    uint t = i > 0u ? i : 1u;
    uint limit = at;
    while (t <= limit && t < rn) {
        if (r_mergeable(t - 1u, t)) {
            rv[rb + t - 1u].y += r_len(t);
            r_shift_down(t + 1u, rn - (t + 1u), 1u);
            rn--;
            limit--;
        } else {
            t++;
        }
    }
    return true;
}

// ---- Bulk writes ---------------------------------------------------------------------------
// An operation whose pieces are disjoint can be applied in any order, so its pieces are sorted and
// merged into the runs in one pass that writes the result to the other run array, which then becomes
// the target. The cost is the runs plus the pieces, where a
// piece-by-piece write costs the runs for every piece. The result is the same runs, so this is an
// optimisation and not a different model. The repair of a badly fragmented program is such an
// operation, and has thousands of pieces.
#define BULK_MIN 8u

uint bk_alt;
uint bk_rn;
uint bk_fail;
// The first position not yet in the merged runs, and the old run that holds it.
uint bk_cur;
uint bk_old;

// The merged run most recently produced is held in registers, not in memory, until a different run
// follows it. Reading it back from memory after storing it would make every push wait on the store
// before it.
uvec4 bk_last;
uint bk_have;

void bk_push(uint start, uint len, uint k, uint off) {
    if (len == 0u || bk_fail != 0u) {
        return;
    }
    if (bk_have != 0u) {
        if (bk_last.z == k && (k == uint(O_ZERO) || bk_last.w + bk_last.y == off)) {
            bk_last.y += len;
            return;
        }
        if (bk_rn >= uint(RUN_CAP)) {
            bk_fail = 1u;
            return;
        }
        rv[bk_alt + bk_rn] = bk_last;
        bk_rn++;
    }
    bk_last = uvec4(start, len, k, off);
    bk_have = 1u;
}

// Store the run still held in registers.
void bk_flush() {
    if (bk_have != 0u) {
        if (bk_rn >= uint(RUN_CAP)) {
            bk_fail = 1u;
        } else {
            rv[bk_alt + bk_rn] = bk_last;
            bk_rn++;
        }
        bk_have = 0u;
    }
}

// Carry the old runs' bytes of [bk_cur, upto) over, and move the cursor to `upto`.
void bk_copy_old(uint upto) {
    while (bk_cur < upto && bk_fail == 0u) {
        uvec4 run = rv[rb + bk_old];
        uint rs = run.x;
        uint re = rs + run.y;
        uint end = min(re, upto);
        uint off = run.z == uint(O_ZERO) ? 0u : run.w + (bk_cur - rs);
        bk_push(bk_cur, end - bk_cur, run.z, off);
        bk_cur = end;
        if (bk_cur == re) {
            bk_old++;
        }
    }
}

// Write the piece [s, s + n), which starts at or after the cursor.
void bk_piece(uint s, uint n, uint k, uint off) {
    bk_copy_old(s);
    bk_push(s, n, k, off);
    bk_cur = s + n;
    while (bk_old < rn) {
        uvec4 run = rv[rb + bk_old];
        if (run.x + run.y > bk_cur) {
            break;
        }
        bk_old++;
    }
}

// The pieces of an operation are collected in the third run array, as one vector each:
// {start, length, origin kind, origin offset}.
uint bk_scr;
uint bk_m;

// Collect the pieces operation `i` writes, in declared order. A copy's pieces come from pairing its
// two streams. Returns false when there are more than the array holds.
bool bk_collect(uint sb, uint i, uint kind) {
    uint df = o_dfirst(sb, i);
    uint dn = o_dn(sb, i);
    bk_scr = rb_home + uint(2 * RUN_CAP);
    bk_m = 0u;
    if (kind == uint(K_SOURCE)) {
        uint sf = o_sfirst(sb, i);
        uint sn = o_sn(sb, i);
        uint de = 0u;
        uint se = 0u;
        uint doff = 0u;
        uint soff = 0u;
        while (de < dn && se < sn) {
            uint dl = x_len(sb, df + de);
            uint sl = x_len(sb, sf + se);
            uint n = min(dl - doff, sl - soff);
            if (bk_m >= uint(RUN_CAP)) {
                return false;
            }
            rv[bk_scr + bk_m] = uvec4(x_start(sb, df + de) + doff, n, uint(O_OLD), x_start(sb, sf + se) + soff);
            bk_m++;
            doff += n;
            soff += n;
            if (doff == dl) {
                de++;
                doff = 0u;
            }
            if (soff == sl) {
                se++;
                soff = 0u;
            }
        }
        return true;
    }
    for (uint e = 0u; e < dn; e++) {
        uint s = x_start(sb, df + e);
        rv[bk_scr + e] = kind == uint(K_REPLACE)
            ? uvec4(s, x_len(sb, df + e), uint(O_NEW), s)
            : uvec4(s, x_len(sb, df + e), uint(O_ZERO), 0u);
    }
    bk_m = dn;
    return true;
}

// Sort the collected pieces by start with a bottom-up merge sort, passing them between the third run
// array and the one the replay is not using, which is free until the merge writes to it. A rotated or
// shuffled operation costs about log2(n) passes over its pieces.
void bk_sort() {
    uint a = bk_scr;
    uint b = rb == rb_home ? rb_home + uint(RUN_CAP) : rb_home;
    uint passes = 0u;
    for (uint width = 1u; width < bk_m; width <<= 1u) {
        for (uint lo = 0u; lo < bk_m; lo += 2u * width) {
            uint mid = min(lo + width, bk_m);
            uint hi = min(lo + 2u * width, bk_m);
            uint i = lo;
            uint j = mid;
            uint o = lo;
            while (i < mid && j < hi) {
                uvec4 x = rv[a + i];
                uvec4 y = rv[a + j];
                if (y.x < x.x) {
                    rv[b + o] = y;
                    j++;
                } else {
                    rv[b + o] = x;
                    i++;
                }
                o++;
            }
            while (i < mid) {
                rv[b + o] = rv[a + i];
                i++;
                o++;
            }
            while (j < hi) {
                rv[b + o] = rv[a + j];
                j++;
                o++;
            }
        }
        uint t = a;
        a = b;
        b = t;
        passes++;
    }
    if (a != bk_scr) {
        for (uint k = 0u; k < bk_m; k++) {
            rv[bk_scr + k] = rv[a + k];
        }
    }
    rwork += bk_m * passes;
}

// Put the collected pieces in ascending order, and say whether they are disjoint. Pieces that overlap
// write their overlap in declared order, which sorting would change, so they are not merged.
bool bk_order() {
    bool ascending = true;
    for (uint k = 1u; k < bk_m; k++) {
        if (rv[bk_scr + k].x < rv[bk_scr + k - 1u].x) {
            ascending = false;
            break;
        }
    }
    if (!ascending) {
        if (bk_m > uint(SORT_MAX)) {
            return false;
        }
        bk_sort();
    }
    for (uint k = 1u; k < bk_m; k++) {
        uvec4 before = rv[bk_scr + k - 1u];
        if (before.x + before.y > rv[bk_scr + k].x) {
            return false;
        }
    }
    return true;
}

// Apply the collected, ordered, disjoint pieces in one pass. Returns S_OK, or S_RUNCAP when the
// merged runs do not fit.
uint r_bulk(uint new_len) {
    bk_alt = rb == rb_home ? rb_home + uint(RUN_CAP) : rb_home;
    bk_rn = 0u;
    bk_fail = 0u;
    bk_cur = 0u;
    bk_old = 0u;
    bk_have = 0u;
    for (uint k = 0u; k < bk_m; k++) {
        uvec4 piece = rv[bk_scr + k];
        bk_piece(piece.x, piece.y, piece.z, piece.w);
    }
    bk_copy_old(new_len);
    bk_flush();
    if (bk_fail != 0u) {
        return uint(S_RUNCAP);
    }
    rwork += rn + bk_rn;
    rb = bk_alt;
    rn = bk_rn;
    return uint(S_OK);
}

// Leave the runs in the candidate's own array, where the scan stages look for them.
void r_settle() {
    if (rb == rb_home) {
        return;
    }
    for (uint r = 0u; r < rn; r++) {
        rv[rb_home + r] = rv[rb + r];
    }
    rb = rb_home;
}

// ---- One operation's own rules -------------------------------------------------------------
// The client enforces these as well, so a program that breaks one could not be applied: extents
// are non-empty and inside the image they address, a copy's streams are equal in length, and an
// in-target copy's source meets none of its destinations.

uint chk_total;

// Why a write was refused: the work budget, or the run arrays.
uint r_why() {
    return rover != 0u ? uint(S_WORK) : uint(S_RUNCAP);
}

uint check_op(uint sb, uint i, uint new_len, uint old_len) {
    uint kind = o_kind(sb, i);
    uint df = o_dfirst(sb, i);
    uint dn = o_dn(sb, i);
    uint sf = o_sfirst(sb, i);
    uint sn = o_sn(sb, i);
    if (dn == 0u) {
        return uint(S_STRUCT);
    }
    if (dn + sn > P(P_MAX_EXT_OP)) {
        return uint(S_LIMIT);
    }
    uint dsum = 0u;
    for (uint e = 0u; e < dn; e++) {
        uint s = x_start(sb, df + e);
        uint l = x_len(sb, df + e);
        if (l == 0u || s > new_len || l > new_len - s) {
            return uint(S_STRUCT);
        }
        if (!sum_add(dsum, l)) {
            return uint(S_OVERFLOW);
        }
    }
    chk_total = dsum;
    if (!k_is_copy(kind)) {
        return sn == 0u ? uint(S_OK) : uint(S_STRUCT);
    }
    if (sn == 0u) {
        return uint(S_STRUCT);
    }
    uint bound = kind == uint(K_SOURCE) ? old_len : new_len;
    uint ssum = 0u;
    for (uint e = 0u; e < sn; e++) {
        uint s = x_start(sb, sf + e);
        uint l = x_len(sb, sf + e);
        if (l == 0u || s > bound || l > bound - s) {
            return uint(S_STRUCT);
        }
        if (!sum_add(ssum, l)) {
            return uint(S_OVERFLOW);
        }
    }
    if (ssum != dsum) {
        return uint(S_STRUCT);
    }
    if (kind == uint(K_TARGET)) {
        if (P(P_READABLE) == 0u) {
            return uint(S_STRUCT);
        }
        // Every source extent is tested against every destination extent, so the test is quadratic
        // and counts against the replay's work.
        if (dn * sn > uint(REPLAY_BUDGET)) {
            return uint(S_WORK);
        }
        rwork += dn * sn;
        for (uint a = 0u; a < sn; a++) {
            uint ss = x_start(sb, sf + a);
            uint se = ss + x_len(sb, sf + a);
            for (uint b = 0u; b < dn; b++) {
                uint ds = x_start(sb, df + b);
                uint de = ds + x_len(sb, df + b);
                if (ss < de && ds < se) {
                    return uint(S_STRUCT);
                }
            }
        }
    }
    return uint(S_OK);
}

// Replay the program in slot `sb` on the runs at `rb`. Returns S_OK, or the reason the program is
// refused. On success `blob` is the data bytes the program carries.
uint replay(uint sb, out uint blob) {
    uint new_len = P(P_NEW_LEN);
    uint old_len = P(P_OLD_LEN);
    uint nops = p_nops(sb);
    r_reset(new_len);
    blob = 0u;
    for (uint i = 0u; i < nops; i++) {
        uint why = check_op(sb, i, new_len, old_len);
        if (why != uint(S_OK)) {
            return why;
        }
        uint kind = o_kind(sb, i);
        uint df = o_dfirst(sb, i);
        uint dn = o_dn(sb, i);
        if (kind == uint(K_REPLACE) && !sum_add(blob, chk_total)) {
            return uint(S_OVERFLOW);
        }
        if (kind != uint(K_TARGET) && dn >= BULK_MIN && bk_collect(sb, i, kind) && bk_order()) {
            uint why_bulk = r_bulk(new_len);
            if (why_bulk != uint(S_OK)) {
                return why_bulk;
            }
            continue;
        }
        if (kind == uint(K_REPLACE)) {
            for (uint e = 0u; e < dn; e++) {
                uint s = x_start(sb, df + e);
                if (!r_write(s, x_len(sb, df + e), uint(O_NEW), s)) {
                    return r_why();
                }
            }
            continue;
        }
        if (kind == uint(K_ZERO)) {
            for (uint e = 0u; e < dn; e++) {
                if (!r_write(x_start(sb, df + e), x_len(sb, df + e), uint(O_ZERO), 0u)) {
                    return r_why();
                }
            }
            continue;
        }
        uint sf = o_sfirst(sb, i);
        uint sn = o_sn(sb, i);
        if (kind == uint(K_TARGET)) {
            // A copy out of the target may not read bytes whose contents are unknown.
            for (uint e = 0u; e < sn; e++) {
                uint p = x_start(sb, sf + e);
                uint end = p + x_len(sb, sf + e);
                while (p < end) {
                    uint r = r_find(p);
                    uint re = min(end, r_start(r) + r_len(r));
                    if (r_kind(r) == uint(O_OLD)) {
                        uint x = r_off(r) + (p - r_start(r));
                        if (x + (re - p) > old_len) {
                            return uint(S_TARGET);
                        }
                    }
                    p = re;
                }
            }
        }
        // The two streams are paired by stream position, piece by piece.
        uint de = 0u;
        uint se = 0u;
        uint doff = 0u;
        uint soff = 0u;
        while (de < dn && se < sn) {
            uint dl = x_len(sb, df + de);
            uint sl = x_len(sb, sf + se);
            uint n = min(dl - doff, sl - soff);
            uint dpos = x_start(sb, df + de) + doff;
            uint spos = x_start(sb, sf + se) + soff;
            if (kind == uint(K_SOURCE)) {
                if (!r_write(dpos, n, uint(O_OLD), spos)) {
                    return r_why();
                }
            } else {
                uint p = spos;
                uint end = spos + n;
                while (p < end) {
                    uint r = r_find(p);
                    uint re = min(end, r_start(r) + r_len(r));
                    uint k = r_kind(r);
                    uint o = k == uint(O_ZERO) ? 0u : r_off(r) + (p - r_start(r));
                    if (!r_write(dpos + (p - spos), re - p, k, o)) {
                        return r_why();
                    }
                    p = re;
                }
            }
            doff += n;
            soff += n;
            if (doff == dl) {
                de++;
                doff = 0u;
            }
            if (soff == sl) {
                se++;
                soff = 0u;
            }
        }
    }
    return uint(S_OK);
}
