// The genetic operators, one thread per child.
//
// Each move reads a parent slot and writes a child slot through the writer of `program.glsl`, so a
// child is always packed, and a capacity that runs out rejects the child with its reason instead
// of truncating it. A move returns false when it has no valid form for its parent; the child is
// then an unchanged copy of nothing and is skipped. Parents are never written, so children can be
// bred in any order, and every random choice comes from the child's own stream `R`.
//
// A proposal is a change, not a guarantee: whether the child reproduces the new image is decided
// by the evaluation that follows, which also repairs it. The moves that edit a copy's source or
// length keep the two streams of a copy equal, because the replay refuses a copy whose streams
// differ. Compression levels and the framed and sparse encodings are not in the chromosome: the
// host chooses them after the search.

uint R;

// Bytes the proposal being made may still compare. Every comparison draws on it, so one child cannot
// hold up a whole generation with a long match: a copy longer than the budget is built from
// several proposals and joined by compaction.
uint budget;

uint below(uint n) {
    return rng_below(R, n);
}

bool pct(uint p) {
    return rng_percent(R, p);
}

// ---- Where a change can start --------------------------------------------------------------
// A position drawn uniformly from the cells whose new bytes differ from the old, which reaches
// places that no data operation covers.
bool sample_changed(out uint pos) {
    uint n = P(P_NCHANGED);
    pos = 0u;
    if (n == 0u) {
        return false;
    }
    uint g = ix[P(P_CHANGED) + below(n)];
    uint base = g * uint(GRID);
    uint width = min(uint(GRID), P(P_NEW_LEN) - base);
    pos = base + below(width);
    return true;
}

// A position drawn uniformly from the bytes the program writes raw.
bool data_position(uint par, out uint pos) {
    uint nops = p_nops(par);
    uint total = 0u;
    for (uint i = 0u; i < nops; i++) {
        if (o_kind(par, i) == uint(K_REPLACE)) {
            sum_add(total, o_stream(par, i));
        }
    }
    pos = 0u;
    if (total == 0u) {
        return false;
    }
    uint r = rng_next(R) % total;
    for (uint i = 0u; i < nops; i++) {
        if (o_kind(par, i) != uint(K_REPLACE)) {
            continue;
        }
        uint first = o_dfirst(par, i);
        uint n = o_dn(par, i);
        for (uint e = 0u; e < n; e++) {
            uint len = x_len(par, first + e);
            if (r < len) {
                pos = x_start(par, first + e) + r;
                return true;
            }
            r -= len;
        }
    }
    return false;
}

// ---- Comparing images ----------------------------------------------------------------------
uint src_len(bool tgt) {
    return tgt ? P(P_NEW_LEN) : P(P_OLD_LEN);
}

// A byte or word of the source image: the new image when `tgt`, else the old. The two images are
// one buffer, so this chooses an offset, not a buffer.
uint src_byte(bool tgt, uint i) {
    return (img[(tgt ? new_base() : 0u) + (i >> 2u)] >> ((i & 3u) << 3u)) & 0xffu;
}

uint src_u32(bool tgt, uint i) {
    return img_u32(tgt ? new_base() : 0u, i);
}

// How many bytes from `x` in the source image equal the new image's bytes from `p`, up to `limit`
// and the end of the source. A word at a time while whole words match.
uint match_len(bool tgt, uint x, uint p, uint limit) {
    uint sl = src_len(tgt);
    if (x >= sl) {
        return 0u;
    }
    uint lim = min(min(limit, sl - x), budget);
    uint n = 0u;
    while (n + 4u <= lim && src_u32(tgt, x + n) == new_u32(p + n)) {
        n += 4u;
    }
    while (n < lim && src_byte(tgt, x + n) == new_byte(p + n)) {
        n++;
    }
    budget -= min(budget, n + 1u);
    return n;
}

// How many zero bytes the new image has from `p`, up to `limit`.
uint zero_run(uint p, uint limit) {
    uint n = 0u;
    while (n + 4u <= limit && new_u32(p + n) == 0u) {
        n += 4u;
    }
    while (n < limit && new_byte(p + n) == 0u) {
        n++;
    }
    return n;
}

// ---- The sorted copy-source tables ---------------------------------------------------------
uint tab_hash(uint base, uint i) {
    return ix[base + 2u * i];
}

uint tab_pos(uint base, uint i) {
    return ix[base + 2u * i + 1u];
}

uint tab_lower(uint base, uint cap, uint h) {
    uint lo = 0u;
    uint hi = cap;
    while (lo < hi) {
        uint mid = (lo + hi) >> 1u;
        if (tab_hash(base, mid) < h) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    return lo;
}

uint tab_upper(uint base, uint cap, uint h) {
    uint lo = 0u;
    uint hi = cap;
    while (lo < hi) {
        uint mid = (lo + hi) >> 1u;
        if (tab_hash(base, mid) <= h) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    return lo;
}

// A guided source for the bytes at `pos`: the longest verified copy among the table entries whose
// hash equals the hash of a probe window at `pos + t`, for each offset `t` below the sampling
// stride. A source position is sampled only every `stride` bytes, so the probe is moved through
// `stride` offsets to meet one, and the source is then extended backwards by `t` bytes to start at
// `pos`. Every candidate is verified byte for byte, so a hash collision costs a comparison and
// nothing else. With `tgt` the source is the new image as it is written, and its range may not
// meet the destination's.
bool guided_source(bool tgt, uint pos, uint room, out uint src, out uint len) {
    src = 0u;
    len = 0u;
    uint nl = P(P_NEW_LEN);
    uint base = tgt ? P(P_NEW_TABLE) : P(P_OLD_TABLE);
    uint cap = tgt ? P(P_NEW_CAP) : P(P_OLD_CAP);
    uint entries = tgt ? P(P_NEW_ENTRIES) : P(P_OLD_ENTRIES);
    uint stride = P(P_IDX_STRIDE);
    if (entries == 0u || pos + uint(PROBE) > nl) {
        return false;
    }
    uint h = 0u;
    for (uint k = 0u; k < uint(PROBE); k++) {
        h = h * uint(HASH_BASE) + new_byte(pos + k);
    }
    uint pw = 1u;
    for (uint k = 1u; k < uint(PROBE); k++) {
        pw *= uint(HASH_BASE);
    }
    uint best_n = 0u;
    uint best_s = 0u;
    uint tried = 0u;
    for (uint t = 0u; t < stride && t < room && pos + t + uint(PROBE) <= nl && tried < uint(CANDIDATES); t++) {
        if (t > 0u) {
            h = (h - new_byte(pos + t - 1u) * pw) * uint(HASH_BASE) + new_byte(pos + t + uint(PROBE) - 1u);
        }
        uint lo = tab_lower(base, cap, h);
        if (lo >= cap || tab_hash(base, lo) != h) {
            continue;
        }
        uint hi = tab_upper(base, cap, h);
        uint count = hi - lo;
        uint take = min(count, 4u);
        uint first = count > take ? below(count) : 0u;
        for (uint k = 0u; k < take && tried < uint(CANDIDATES); k++) {
            uint x = tab_pos(base, lo + (first + k) % count);
            if (x == NO_POS || x < t) {
                continue;
            }
            tried++;
            uint s = x - t;
            uint maxn = room;
            if (tgt) {
                uint gap = s > pos ? s - pos : pos - s;
                if (gap == 0u) {
                    continue;
                }
                maxn = min(room, gap);
            }
            if (t >= maxn) {
                continue;
            }
            uint f = match_len(tgt, x, pos + t, maxn - t);
            if (f < uint(PROBE)) {
                continue;
            }
            if (t > 0u && match_len(tgt, s, pos, t) < t) {
                continue;
            }
            if (t + f > best_n) {
                best_n = t + f;
                best_s = s;
            }
        }
    }
    if (best_n < uint(PROBE)) {
        return false;
    }
    src = best_s;
    len = best_n;
    return true;
}

// Sample the positions of [from, to) of the source image whose byte equals `first`, up to `cap` of
// them, uniformly among those seen so far (reservoir sampling). Four bytes are tested per load, and
// a word with no matching byte costs nothing more.
void sample_equal(bool tgt, uint first, uint from, uint to, uint cap, inout uint kept[CANDIDATES], inout uint nk, inout uint seen) {
    uint rep = first * 0x01010101u;
    uint x = from;
    while (x < to) {
        if (x + 4u <= to) {
            uint t = src_u32(tgt, x) ^ rep;
            // A byte of `t` is zero exactly when the source byte equals `first`.
            if (((t - 0x01010101u) & ~t & 0x80808080u) == 0u) {
                x += 4u;
                continue;
            }
        }
        uint stop = min(to, x + 4u);
        for (; x < stop; x++) {
            if (src_byte(tgt, x) == first) {
                seen++;
                if (nk < cap) {
                    kept[nk] = x;
                    nk++;
                } else {
                    uint r = below(seen);
                    if (r < cap) {
                        kept[r] = x;
                    }
                }
            }
        }
    }
}

// A general source for the byte at `pos`: up to CANDIDATES positions of the source image whose
// first byte equals it, sampled from a bounded window of the image that starts at a random place,
// each extended to its real match. Any length from one byte up is reachable, and a copy may
// start anywhere, including near the end of an image. Half the time the longest match is taken,
// and otherwise a random one. The length is the whole match, or a random part of it. With
// `disjoint` a source may not meet the destination.
bool general_match(bool tgt, uint pos, uint room, bool disjoint, out uint src, out uint len) {
    src = 0u;
    len = 0u;
    uint sl = src_len(tgt);
    if (sl == 0u) {
        return false;
    }
    uint first = new_byte(pos);
    uint window = min(sl, uint(SCAN_WINDOW));
    uint start = below(sl);
    uint kept[CANDIDATES];
    uint nk = 0u;
    uint seen = 0u;
    // The window wraps at the end of the image, so it is up to two linear stretches.
    uint first_end = min(start + window, sl);
    sample_equal(tgt, first, start, first_end, uint(CANDIDATES), kept, nk, seen);
    if (start + window > sl) {
        sample_equal(tgt, first, 0u, start + window - sl, uint(CANDIDATES), kept, nk, seen);
    }
    // Each sample is extended to its real match. The longest match and a uniformly random match
    // are tracked as the samples are extended, so no list of matches is kept.
    uint matches = 0u;
    uint long_x = 0u;
    uint long_n = 0u;
    uint rand_x = 0u;
    uint rand_n = 0u;
    for (uint k = 0u; k < nk; k++) {
        uint x = kept[k];
        uint maxn = room;
        if (disjoint) {
            uint gap = x > pos ? x - pos : pos - x;
            maxn = min(room, gap);
        }
        uint n = match_len(tgt, x, pos, maxn);
        if (n < 1u) {
            continue;
        }
        matches++;
        if (n >= long_n) {
            long_x = x;
            long_n = n;
        }
        if (below(matches) == 0u) {
            rand_x = x;
            rand_n = n;
        }
    }
    if (matches == 0u) {
        return false;
    }
    bool longest = pct(50);
    src = longest ? long_x : rand_x;
    uint whole = longest ? long_n : rand_n;
    len = pct(60) ? whole : 1u + below(whole);
    return true;
}

// ---- Moves ---------------------------------------------------------------------------------
// Finish a child that is its parent with some numbers changed in place.
void seal_inplace(uint chd) {
    st[chd + H_FIT] = FIT_INVALID;
    st[chd + H_STATUS] = uint(S_OK);
}

// A guided copy of old bytes: from a probed position, at the longest verified match, up to the end
// of the extent that holds the position. With `tgt` the source is the new image.
bool mv_guided(uint par, uint chd, bool tgt) {
    if (tgt && P(P_READABLE) == 0u) {
        return false;
    }
    budget = uint(MAX_COMPARE);
    uint pos;
    if (!sample_changed(pos)) {
        return false;
    }
    uint i;
    uint a;
    uint end;
    if (!find_op_at(par, pos, i, a, end)) {
        return false;
    }
    uint s;
    uint n;
    if (!guided_source(tgt, pos, end - pos, s, n)) {
        return false;
    }
    if (!tgt && pct(30)) {
        n = uint(PROBE) + below(n - uint(PROBE) + 1u);
    }
    w_begin(chd);
    carve(par, i, a, n, tgt ? uint(K_TARGET) : uint(K_SOURCE), pos, s);
    w_end();
    return true;
}

// A general copy: from a position the program writes as data, or a changed position, to a source
// the bounded scan finds. Reaches copies of any length and offset.
bool mv_short_copy(uint par, uint chd) {
    budget = uint(MAX_COMPARE);
    uint pos;
    bool found = pct(DATA_SHARE) ? data_position(par, pos) : sample_changed(pos);
    if (!found) {
        return false;
    }
    uint i;
    uint a;
    uint end;
    if (!find_op_at(par, pos, i, a, end)) {
        return false;
    }
    bool tgt = pct(P(P_READABLE) != 0u ? uint(TARGET_SHARE) : 0u);
    uint s;
    uint n;
    if (!general_match(tgt, pos, end - pos, tgt, s, n)) {
        return false;
    }
    w_begin(chd);
    carve(par, i, a, n, tgt ? uint(K_TARGET) : uint(K_SOURCE), pos, s);
    w_end();
    return true;
}

// Several copies in a row, then a compaction, so the copies do not spend the operation budget on
// their own. Each copy is guided or general at random. The two scratch slots hold the program
// between steps.
bool mv_compact(uint par, uint chd);

bool mv_sweep(uint par, uint chd, uint scr0, uint scr1) {
    uint attempts = 2u + below(7u);
    uint cur = par;
    bool changed = false;
    uint use = 0u;
    for (uint k = 0u; k < attempts; k++) {
        uint dst = use == 0u ? scr0 : scr1;
        bool ok;
        if (pct(GUIDED_SHARE)) {
            ok = mv_guided(cur, dst, false);
        } else {
            ok = mv_short_copy(cur, dst);
        }
        if (!ok) {
            continue;
        }
        if (st[dst + H_STATUS] != uint(S_OK)) {
            // A capacity ran out: the whole child is refused for that reason.
            p_clear(chd);
            st[chd + H_STATUS] = st[dst + H_STATUS];
            return true;
        }
        cur = dst;
        use ^= 1u;
        changed = true;
    }
    if (!changed) {
        return false;
    }
    return mv_compact(cur, chd);
}

// A copy whose source moves to another place that holds exactly the bytes the copy writes.
bool mv_copy_shift(uint par, uint chd) {
    budget = uint(MAX_COMPARE);
    uint nops = p_nops(par);
    uint copies = 0u;
    for (uint i = 0u; i < nops; i++) {
        uint k = o_kind(par, i);
        if (k == uint(K_SOURCE) || (k == uint(K_TARGET) && P(P_READABLE) != 0u)) {
            copies++;
        }
    }
    if (copies == 0u) {
        return false;
    }
    uint pick = below(copies);
    uint op = 0u;
    for (uint i = 0u; i < nops; i++) {
        uint k = o_kind(par, i);
        if (k == uint(K_SOURCE) || (k == uint(K_TARGET) && P(P_READABLE) != 0u)) {
            if (pick == 0u) {
                op = i;
                break;
            }
            pick--;
        }
    }
    bool tgt = o_kind(par, op) == uint(K_TARGET);
    uint want = o_stream(par, op);
    uint sl = src_len(tgt);
    if (want == 0u || want > sl) {
        return false;
    }
    uint df = o_dfirst(par, op);
    uint dn = o_dn(par, op);
    uint domain = sl - want + 1u;
    uint window = min(domain, uint(SCAN_WINDOW));
    uint start = below(domain);
    uint chosen = 0u;
    uint valid = 0u;
    uint first_byte = new_byte(x_start(par, df));
    for (uint v = 0u; v < window && budget > 0u; v++) {
        uint x = start + v;
        if (x >= domain) {
            x -= domain;
        }
        if (src_byte(tgt, x) != first_byte) {
            continue;
        }
        bool ok = true;
        if (tgt) {
            for (uint e = 0u; e < dn; e++) {
                uint ds = x_start(par, df + e);
                if (x < ds + x_len(par, df + e) && ds < x + want) {
                    ok = false;
                    break;
                }
            }
        }
        // Each candidate is compared in full, within a total byte budget for the proposal.
        uint off = 0u;
        for (uint e = 0u; e < dn && ok; e++) {
            uint len = x_len(par, df + e);
            if (match_len(tgt, x + off, x_start(par, df + e), len) != len) {
                ok = false;
            }
            off += len;
        }
        if (ok) {
            valid++;
            if (below(valid) == 0u) {
                chosen = x;
            }
        }
    }
    if (valid == 0u) {
        return false;
    }
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        if (i != op) {
            w_copy_op(par, i);
            continue;
        }
        w_open(o_kind(par, i));
        for (uint e = 0u; e < dn; e++) {
            w_dst(x_start(par, df + e), x_len(par, df + e));
        }
        w_src(chosen, want);
    }
    w_end();
    return true;
}

// A zero operation over a run that is zero in the new image. The start comes from a bounded scan
// of the whole image, so zero bytes inside a changed cell are proposed as well as whole zero cells.
bool mv_zero_create(uint par, uint chd) {
    uint nl = P(P_NEW_LEN);
    if (nl == 0u) {
        return false;
    }
    uint window = min(nl, uint(SCAN_WINDOW));
    uint start = below(nl);
    uint seen = 0u;
    uint kept[CANDIDATES];
    uint nk = 0u;
    sample_equal(true, 0u, start, min(start + window, nl), 1u, kept, nk, seen);
    if (start + window > nl) {
        sample_equal(true, 0u, 0u, start + window - nl, 1u, kept, nk, seen);
    }
    if (seen == 0u) {
        return false;
    }
    uint pos = kept[0];
    uint i;
    uint a;
    uint end;
    if (!find_op_at(par, pos, i, a, end)) {
        return false;
    }
    uint n = zero_run(pos, min(end - pos, uint(MAX_ZERO_RUN)));
    if (pct(30)) {
        n = 1u + below(n);
    }
    w_begin(chd);
    carve(par, i, a, n, uint(K_ZERO), pos, 0u);
    w_end();
    return true;
}

// Insert a zero operation over a random range at a random place. The mismatches it leaves are
// repaired by the evaluation, so this proposes a zero-then-patch plan.
bool mv_insert_zero(uint par, uint chd) {
    uint nl = P(P_NEW_LEN);
    if (nl == 0u) {
        return false;
    }
    uint nops = p_nops(par);
    uint start = below(nl);
    uint n = rng_log_length(R, nl - start);
    uint at = below(nops + 1u);
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        if (i == at) {
            w_simple(uint(K_ZERO), start, n, 0u);
        }
        w_copy_op(par, i);
    }
    if (at == nops) {
        w_simple(uint(K_ZERO), start, n, 0u);
    }
    w_end();
    return true;
}

// Delete one operation. The repair covers whatever it leaves unwritten.
bool mv_remove(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops == 0u) {
        return false;
    }
    uint gone = below(nops);
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        if (i != gone) {
            w_copy_op(par, i);
        }
    }
    w_end();
    return true;
}

// Exchange two adjacent operations. Order matters when their extents overlap.
bool mv_swap(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops < 2u) {
        return false;
    }
    uint at = below(nops - 1u);
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        uint from = i;
        if (i == at) {
            from = at + 1u;
        } else if (i == at + 1u) {
            from = at;
        }
        w_copy_op(par, from);
    }
    w_end();
    return true;
}

// Split one operation at a random stream position into two of the same kind. A copy's source
// stream is cut at the same position, so both halves stay copies of equal length.
bool mv_split(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops == 0u) {
        return false;
    }
    uint i = below(nops);
    uint total = o_stream(par, i);
    if (total < 2u) {
        return false;
    }
    uint at = 1u + below(total - 1u);
    w_begin(chd);
    for (uint k = 0u; k < nops; k++) {
        if (k == i) {
            w_part(par, k, 0u, at);
            w_part(par, k, at, total);
        } else {
            w_copy_op(par, k);
        }
    }
    w_end();
    return true;
}

// Join two adjacent operations of one kind into one.
bool mv_merge(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops < 2u) {
        return false;
    }
    uint i = below(nops - 1u);
    if (o_kind(par, i) != o_kind(par, i + 1u)) {
        return false;
    }
    w_begin(chd);
    for (uint k = 0u; k < nops; k++) {
        if (k == i) {
            w_open(o_kind(par, k));
            for (uint side = 0u; side < 2u; side++) {
                uint dn = o_dn(par, k + side);
                uint df = o_dfirst(par, k + side);
                for (uint e = 0u; e < dn; e++) {
                    w_dst_join(x_start(par, df + e), x_len(par, df + e));
                }
            }
            for (uint side = 0u; side < 2u; side++) {
                uint sn = o_sn(par, k + side);
                uint sf = o_sfirst(par, k + side);
                for (uint e = 0u; e < sn; e++) {
                    w_src_join(x_start(par, sf + e), x_len(par, sf + e));
                }
            }
            k++;
        } else {
            w_copy_op(par, k);
        }
    }
    w_end();
    return true;
}

// The length of an extent after a change of `step` in the chosen direction, or 0 when the result
// is empty, overflows, or leaves the image of `limit` bytes.
uint resized(uint start, uint len, bool grow, uint step, uint limit) {
    uint n;
    if (grow) {
        if (step > 0xFFFFFFFFu - len) {
            return 0u;
        }
        n = len + step;
    } else {
        if (step >= len) {
            return 0u;
        }
        n = len - step;
    }
    if (start > limit || n > limit - start) {
        return 0u;
    }
    return n;
}

// Change the length of one destination extent by a small power of two. A copy's last destination
// and last source extents change by the same amount, since each stream ends with its last extent,
// so the two streams stay equal in length.
bool mv_shift(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops == 0u) {
        return false;
    }
    uint i = below(nops);
    uint dn = o_dn(par, i);
    if (dn == 0u) {
        return false;
    }
    uint step = 1u << below(12u);
    bool grow = pct(50);
    uint nl = P(P_NEW_LEN);
    uint kind = o_kind(par, i);
    uint patch_a;
    uint new_a;
    uint patch_b = 0xFFFFFFFFu;
    uint new_b = 0u;
    if (k_is_copy(kind)) {
        uint sn = o_sn(par, i);
        if (sn == 0u) {
            return false;
        }
        patch_a = o_dfirst(par, i) + dn - 1u;
        patch_b = o_sfirst(par, i) + sn - 1u;
        new_a = resized(x_start(par, patch_a), x_len(par, patch_a), grow, step, nl);
        uint limit = kind == uint(K_SOURCE) ? P(P_OLD_LEN) : nl;
        new_b = resized(x_start(par, patch_b), x_len(par, patch_b), grow, step, limit);
        if (new_a == 0u || new_b == 0u) {
            return false;
        }
    } else {
        uint k = below(dn);
        patch_a = o_dfirst(par, i) + k;
        new_a = resized(x_start(par, patch_a), x_len(par, patch_a), grow, step, nl);
        if (new_a == 0u) {
            return false;
        }
    }
    p_copy(par, chd);
    st[chd + uint(SLOT_EXT) + patch_a * 2u + 1u] = new_a;
    if (patch_b != 0xFFFFFFFFu) {
        st[chd + uint(SLOT_EXT) + patch_b * 2u + 1u] = new_b;
    }
    seal_inplace(chd);
    return true;
}

// Move one destination extent's start by a small power of two, keeping its length.
bool mv_shift_start(uint par, uint chd) {
    uint nops = p_nops(par);
    if (nops == 0u) {
        return false;
    }
    uint i = below(nops);
    uint dn = o_dn(par, i);
    if (dn == 0u) {
        return false;
    }
    uint e = o_dfirst(par, i) + below(dn);
    uint start = x_start(par, e);
    uint len = x_len(par, e);
    uint step = 1u << below(12u);
    uint moved;
    if (pct(50)) {
        if (step > 0xFFFFFFFFu - start) {
            return false;
        }
        moved = start + step;
    } else {
        if (step > start) {
            return false;
        }
        moved = start - step;
    }
    uint nl = P(P_NEW_LEN);
    if (moved > nl || len > nl - moved) {
        return false;
    }
    p_copy(par, chd);
    st[chd + uint(SLOT_EXT) + e * 2u] = moved;
    seal_inplace(chd);
    return true;
}

// Move one destination extent from a raw operation into another one.
bool mv_regroup(uint par, uint chd) {
    uint nops = p_nops(par);
    uint donors = 0u;
    uint receivers = 0u;
    for (uint i = 0u; i < nops; i++) {
        if (o_kind(par, i) == uint(K_REPLACE)) {
            receivers++;
            if (o_dn(par, i) >= 2u) {
                donors++;
            }
        }
    }
    if (donors == 0u || receivers < 2u) {
        return false;
    }
    uint from = 0u;
    uint to = 0u;
    uint pick = below(donors);
    uint pick_to = below(receivers);
    for (uint i = 0u; i < nops; i++) {
        if (o_kind(par, i) != uint(K_REPLACE)) {
            continue;
        }
        if (o_dn(par, i) >= 2u) {
            if (pick == 0u) {
                from = i;
            }
            pick--;
        }
        if (pick_to == 0u) {
            to = i;
        }
        pick_to--;
    }
    if (from == to) {
        return false;
    }
    uint k = below(o_dn(par, from));
    uint moved = o_dfirst(par, from) + k;
    uint at = below(o_dn(par, to) + 1u);
    uint ms = x_start(par, moved);
    uint ml = x_len(par, moved);
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        if (i != from && i != to) {
            w_copy_op(par, i);
            continue;
        }
        uint dn = o_dn(par, i);
        uint df = o_dfirst(par, i);
        w_open(uint(K_REPLACE));
        for (uint e = 0u; e < dn; e++) {
            if (i == to && e == at) {
                w_dst(ms, ml);
            }
            if (i == from && e == k) {
                continue;
            }
            w_dst(x_start(par, df + e), x_len(par, df + e));
        }
        if (i == to && at == dn) {
            w_dst(ms, ml);
        }
    }
    w_end();
    return true;
}

// Rotate the destination extents of an operation with two or more of them. A copy's destination
// rotates alone: the pairing of its streams is by position, so the copy stays valid and only its
// mapping changes.
bool mv_reorder(uint par, uint chd) {
    uint nops = p_nops(par);
    uint multi = 0u;
    for (uint i = 0u; i < nops; i++) {
        if (o_dn(par, i) >= 2u) {
            multi++;
        }
    }
    if (multi == 0u) {
        return false;
    }
    uint pick = below(multi);
    uint op = 0u;
    for (uint i = 0u; i < nops; i++) {
        if (o_dn(par, i) >= 2u) {
            if (pick == 0u) {
                op = i;
                break;
            }
            pick--;
        }
    }
    uint dn = o_dn(par, op);
    uint r = 1u + below(dn - 1u);
    uint df = o_dfirst(par, op);
    w_begin(chd);
    for (uint i = 0u; i < nops; i++) {
        if (i != op) {
            w_copy_op(par, i);
            continue;
        }
        w_open(o_kind(par, i));
        for (uint e = 0u; e < dn; e++) {
            uint from = df + (e + r) % dn;
            w_dst(x_start(par, from), x_len(par, from));
        }
        uint sn = o_sn(par, i);
        uint sf = o_sfirst(par, i);
        for (uint e = 0u; e < sn; e++) {
            w_src(x_start(par, sf + e), x_len(par, sf + e));
        }
    }
    w_end();
    return true;
}

// Join the source copies into one operation at the front, in program order, and adjacent raw
// operations into one. A source copy reads only the immutable old image, so copies run the same in one
// operation as in many, and joining them saves each operation's header and source hash. Moving a copy
// to the front is not free, though: a copy that comes after another operation writing the same bytes
// would then be overwritten by it. So a copy moves only when its destination lies outside the bounds
// of every earlier operation that stays where it is, and one that might conflict stays in place, as
// its own operation. An operation's bounds are one interval, which is checked first; only when they
// meet are the extents inside compared, a bounded number of times per compaction. The evaluation
// decides everything else.
bool mv_compact(uint par, uint chd) {
    uint nops = p_nops(par);
    uint movable[OP_CAP / 32];
    for (uint w = 0u; w < uint(OP_CAP / 32); w++) {
        movable[w] = 0u;
    }
    // The bounds of the destinations of the operations that do not move, in program order.
    uint stay_lo[OP_CAP];
    uint stay_hi[OP_CAP];
    uint stay_op[OP_CAP];
    uint stays = 0u;
    uint compares = 0u;
    bool any = false;
    for (uint i = 0u; i < nops; i++) {
        uint df = o_dfirst(par, i);
        uint dn = o_dn(par, i);
        uint lo = 0xFFFFFFFFu;
        uint hi = 0u;
        for (uint e = 0u; e < dn; e++) {
            lo = min(lo, x_start(par, df + e));
            hi = max(hi, x_start(par, df + e) + x_len(par, df + e));
        }
        bool moves = o_kind(par, i) == uint(K_SOURCE);
        for (uint s = 0u; moves && s < stays; s++) {
            if (!(lo < stay_hi[s] && stay_lo[s] < hi)) {
                continue;
            }
            // The bounds meet, but the extents inside them may not. Compare them, within a budget;
            // what the budget does not cover is taken to conflict.
            uint j = stay_op[s];
            uint jf = o_dfirst(par, j);
            uint jn = o_dn(par, j);
            for (uint f = 0u; moves && f < jn; f++) {
                uint a = x_start(par, jf + f);
                uint b = a + x_len(par, jf + f);
                if (!(a < hi && lo < b)) {
                    continue;
                }
                for (uint e = 0u; moves && e < dn; e++) {
                    uint c = x_start(par, df + e);
                    uint d = c + x_len(par, df + e);
                    compares++;
                    moves = !(c < b && a < d) && compares <= uint(COMPACT_COMPARES);
                }
            }
        }
        if (moves) {
            movable[i / 32u] |= 1u << (i % 32u);
            any = true;
        } else {
            stay_lo[stays] = lo;
            stay_hi[stays] = hi;
            stay_op[stays] = i;
            stays++;
        }
    }
    w_begin(chd);
    if (any) {
        w_open(uint(K_SOURCE));
        for (uint i = 0u; i < nops; i++) {
            if ((movable[i / 32u] & (1u << (i % 32u))) != 0u) {
                uint dn = o_dn(par, i);
                uint df = o_dfirst(par, i);
                for (uint e = 0u; e < dn; e++) {
                    w_dst_join(x_start(par, df + e), x_len(par, df + e));
                }
            }
        }
        for (uint i = 0u; i < nops; i++) {
            if ((movable[i / 32u] & (1u << (i % 32u))) != 0u) {
                uint sn = o_sn(par, i);
                uint sf = o_sfirst(par, i);
                for (uint e = 0u; e < sn; e++) {
                    w_src_join(x_start(par, sf + e), x_len(par, sf + e));
                }
            }
        }
    }
    uint last = 0xFFFFFFFFu;
    for (uint i = 0u; i < nops; i++) {
        uint kind = o_kind(par, i);
        if ((movable[i / 32u] & (1u << (i % 32u))) != 0u) {
            continue;
        }
        if (kind == uint(K_REPLACE) && last == uint(K_REPLACE)) {
            // The open raw operation takes this one's extents.
            uint dn = o_dn(par, i);
            uint df = o_dfirst(par, i);
            for (uint e = 0u; e < dn; e++) {
                w_dst_join(x_start(par, df + e), x_len(par, df + e));
            }
            continue;
        }
        w_copy_op(par, i);
        last = kind;
    }
    w_end();
    return true;
}

// ---- Programs from scratch -----------------------------------------------------------------
// The program that writes the whole new image in one raw operation.
void make_raw(uint chd) {
    w_begin(chd);
    uint nl = P(P_NEW_LEN);
    if (nl > 0u) {
        w_simple(uint(K_REPLACE), 0u, nl, 0u);
    }
    w_end();
}

// The program that writes only what differs: one operation per run of changed cells, zeros where
// the new bytes are zero. Unchanged runs need nothing at all.
void make_compact(uint chd) {
    w_begin(chd);
    uint nl = P(P_NEW_LEN);
    uint cells = P(P_NCELLS);
    uint classes = P(P_CLASSES);
    uint kind = 0xFFFFFFFFu;
    uint run_start = 0u;
    uint run_end = 0u;
    for (uint g = 0u; g <= cells; g++) {
        uint next = 0xFFFFFFFFu;
        uint start = g * uint(GRID);
        uint end = min(start + uint(GRID), nl);
        if (g < cells) {
            uint cls = ix[classes + g];
            if ((cls & uint(CELL_EQUAL)) == 0u) {
                next = (cls & uint(CELL_ZERO)) != 0u ? uint(K_ZERO) : uint(K_REPLACE);
            }
        }
        if (next == kind && next != 0xFFFFFFFFu) {
            run_end = end;
            continue;
        }
        if (kind != 0xFFFFFFFFu) {
            w_simple(kind, run_start, run_end - run_start, 0u);
        }
        kind = next;
        run_start = start;
        run_end = end;
    }
    w_end();
}

// A greedy seed: make one guided old-image copy proposal per changed cell. Evaluation repairs
// uncovered bytes, so the seed stays bounded while still using the source table's long matches.
void make_greedy(uint chd) {
    w_begin(chd);
    uint nl = P(P_NEW_LEN);
    uint cells = P(P_NCELLS);
    uint classes = P(P_CLASSES);
    for (uint g = 0u; g < cells; g++) {
        uint cls = ix[classes + g];
        if ((cls & uint(CELL_EQUAL)) != 0u) {
            continue;
        }
        uint pos = g * uint(GRID);
        uint end = min(pos + uint(GRID), nl);
        uint src;
        uint len;
        if (guided_source(false, pos, end - pos, src, len)) {
            w_simple(uint(K_SOURCE), pos, len, src);
        } else if ((cls & uint(CELL_ZERO)) != 0u) {
            w_simple(uint(K_ZERO), pos, end - pos, 0u);
        } else {
            w_simple(uint(K_REPLACE), pos, end - pos, 0u);
        }
    }
    w_end();
}

// A random program: the image cut into a few random pieces, each written by a random operation.
// A copy piece takes a random source of its own length, from the old image or from the target.
// The evaluation repairs what is wrong.
void make_immigrant(uint chd) {
    uint nl = P(P_NEW_LEN);
    if (nl == 0u) {
        w_begin(chd);
        w_end();
        return;
    }
    uint pieces = 1u + below(min(6u, nl));
    uint cuts[8];
    uint nc = 0u;
    for (uint k = 0u; k + 1u < pieces; k++) {
        cuts[nc] = 1u + below(max(nl - 1u, 1u));
        nc++;
    }
    cuts[nc] = 0u;
    nc++;
    cuts[nc] = nl;
    nc++;
    // Sort and drop duplicates.
    for (uint a = 1u; a < nc; a++) {
        uint v = cuts[a];
        uint b = a;
        while (b > 0u && cuts[b - 1u] > v) {
            cuts[b] = cuts[b - 1u];
            b--;
        }
        cuts[b] = v;
    }
    uint nu = 0u;
    for (uint a = 0u; a < nc; a++) {
        if (nu == 0u || cuts[nu - 1u] != cuts[a]) {
            cuts[nu] = cuts[a];
            nu++;
        }
    }
    uint kinds[6];
    uint starts[6];
    uint lens[6];
    uint srcs[6];
    uint n = 0u;
    for (uint a = 0u; a + 1u < nu && n < 6u; a++) {
        uint start = cuts[a];
        uint len = cuts[a + 1u] - start;
        uint ways = P(P_READABLE) != 0u ? 4u : 3u;
        uint kind = below(ways);
        // A raw operation stands in for a source copy that the old image cannot supply.
        if (kind == uint(K_SOURCE) && P(P_OLD_LEN) < len) {
            kind = uint(K_REPLACE);
        }
        uint src = 0u;
        if (kind == uint(K_SOURCE)) {
            src = below(P(P_OLD_LEN) - len + 1u);
        } else if (kind == uint(K_TARGET)) {
            src = below(nl - len + 1u);
        }
        kinds[n] = kind;
        starts[n] = start;
        lens[n] = len;
        srcs[n] = src;
        n++;
    }
    for (uint a = n; a > 1u; a--) {
        uint b = below(a);
        uint t;
        t = kinds[a - 1u]; kinds[a - 1u] = kinds[b]; kinds[b] = t;
        t = starts[a - 1u]; starts[a - 1u] = starts[b]; starts[b] = t;
        t = lens[a - 1u]; lens[a - 1u] = lens[b]; lens[b] = t;
        t = srcs[a - 1u]; srcs[a - 1u] = srcs[b]; srcs[b] = t;
    }
    w_begin(chd);
    for (uint a = 0u; a < n; a++) {
        w_simple(kinds[a], starts[a], lens[a], srcs[a]);
    }
    w_end();
}

// Prefix of `a` followed by a suffix of `b`.
bool mv_crossover(uint a, uint b, uint chd) {
    uint na = p_nops(a);
    uint nb = p_nops(b);
    uint i = below(na + 1u);
    uint j = below(nb + 1u);
    w_begin(chd);
    for (uint k = 0u; k < i; k++) {
        w_copy_op(a, k);
    }
    for (uint k = j; k < nb; k++) {
        w_copy_op(b, k);
    }
    w_end();
    return true;
}

// The moves and their weights. Crossover and immigration are chosen separately, by percentage.
uint move_weight(uint mv) {
    switch (int(mv)) {
        case MV_SPLIT: return 8u;
        case MV_MERGE: return 8u;
        case MV_SHIFT: return 10u;
        case MV_SHIFT_START: return 6u;
        case MV_COPY_CREATE: return 16u;
        case MV_SHORT_COPY: return 10u;
        case MV_COPY_SHIFT: return 6u;
        case MV_TARGET_CREATE: return 4u;
        case MV_ZERO_CREATE: return 6u;
        case MV_INSERT_ZERO: return 4u;
        case MV_REMOVE: return 5u;
        case MV_SWAP: return 5u;
        case MV_REGROUP: return 5u;
        case MV_REORDER: return 4u;
        case MV_SWEEP: return 16u;
        default: return 8u;
    }
}

uint pick_move() {
    uint total = 0u;
    for (uint m = 0u; m < uint(MV_COUNT); m++) {
        total += move_weight(m);
    }
    uint r = rng_next(R) % total;
    for (uint m = 0u; m < uint(MV_COUNT); m++) {
        uint w = move_weight(m);
        if (r < w) {
            return m;
        }
        r -= w;
    }
    return 0u;
}

bool apply_move(uint mv, uint par, uint chd, uint scr0, uint scr1) {
    switch (int(mv)) {
        case MV_SPLIT: return mv_split(par, chd);
        case MV_MERGE: return mv_merge(par, chd);
        case MV_SHIFT: return mv_shift(par, chd);
        case MV_SHIFT_START: return mv_shift_start(par, chd);
        case MV_COPY_CREATE: return mv_guided(par, chd, false);
        case MV_SHORT_COPY: return mv_short_copy(par, chd);
        case MV_COPY_SHIFT: return mv_copy_shift(par, chd);
        case MV_TARGET_CREATE: return mv_guided(par, chd, true);
        case MV_ZERO_CREATE: return mv_zero_create(par, chd);
        case MV_INSERT_ZERO: return mv_insert_zero(par, chd);
        case MV_REMOVE: return mv_remove(par, chd);
        case MV_SWAP: return mv_swap(par, chd);
        case MV_REGROUP: return mv_regroup(par, chd);
        case MV_REORDER: return mv_reorder(par, chd);
        case MV_SWEEP: return mv_sweep(par, chd, scr0, scr1);
        default: return mv_compact(par, chd);
    }
}
