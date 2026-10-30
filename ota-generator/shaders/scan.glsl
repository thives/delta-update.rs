// Mismatch scan: where the replayed target still differs from the new image.
//
// The target is cut into equal byte ranges, one thread each, so a large image is compared by many
// threads at once. A mismatch span is a maximal run of differing bytes, and it belongs to the
// range that holds its first byte. A first pass counts those spans, their bytes, and the first
// matching byte of each range. A prefix over the ranges turns the counts into output positions,
// and the same scan then writes the spans in ascending order. A span that runs past the end of
// its range ends at the first matching byte of a later range, which the prefix recorded.

uint sc_prev;
uint sc_open;
uint sc_emit;
uint sc_ss;
uint sc_n;
uint sc_bytes;
uint sc_ok;
uint sc_out;
uint sc_sb;

void sc_put(uint start, uint len) {
    uint at = sc_sb + uint(SLOT_EXT) + sc_out * 2u;
    st[at] = start;
    st[at + 1u] = len;
    sc_out++;
}

void sc_good(uint p) {
    if (sc_ok == NO_POS) {
        sc_ok = p;
    }
    if (sc_prev != 0u && sc_emit != 0u && sc_open != 0u) {
        sc_put(sc_ss, p - sc_ss);
        sc_open = 0u;
    }
    sc_prev = 0u;
}

void sc_bad(uint p) {
    sc_bytes++;
    if (sc_prev == 0u) {
        sc_n++;
        if (sc_emit != 0u) {
            sc_ss = p;
            sc_open = 1u;
        }
    }
    sc_prev = 1u;
}

// Whether byte `p`, which run `r` holds, differs from the new image.
bool sc_byte_bad(uint r, uint p) {
    uint k = r_kind(r);
    uint o = r_off(r) + (p - r_start(r));
    if (k == uint(O_ZERO)) {
        return new_byte(p) != 0u;
    }
    if (k == uint(O_NEW)) {
        return new_byte(o) != new_byte(p);
    }
    return !(o < P(P_OLD_LEN) && old_byte(o) == new_byte(p));
}

// Scan the part [p, e) of a run whose bytes are known by a bit map: a set bit at a position means the
// byte there matches. Thirty-two bytes are taken from one word. Within a word, a mismatch starts at
// a bad bit whose predecessor is good, and ends at a good bit whose predecessor is bad, where the
// predecessor of the first bit is the byte before the piece.
void sc_bits(uint p, uint e, uint map) {
    while (p < e) {
        uint w = p >> 5u;
        uint lo = p & 31u;
        uint hi = min(32u, lo + (e - p));
        uint mask = hi == 32u ? (0xFFFFFFFFu << lo) : (((1u << hi) - 1u) & (0xFFFFFFFFu << lo));
        uint bits = ix[map + w];
        uint good = bits & mask;
        uint bad = ~bits & mask;
        uint base = w << 5u;
        if (sc_prev == 0u && bad == 0u) {
            if (sc_ok == NO_POS) {
                sc_ok = base + uint(findLSB(good));
            }
            p = base + hi;
            continue;
        }
        uint carried = sc_prev != 0u ? (1u << lo) : 0u;
        uint before = (bad << 1) | carried;
        uint starts = bad & ~before;
        sc_n += uint(bitCount(starts));
        sc_bytes += uint(bitCount(bad));
        if (sc_ok == NO_POS && good != 0u) {
            sc_ok = base + uint(findLSB(good));
        }
        if (sc_emit != 0u) {
            uint ends = good & before;
            uint events = starts | ends;
            while (events != 0u) {
                uint i = uint(findLSB(events));
                events &= events - 1u;
                if ((starts >> i & 1u) != 0u) {
                    sc_ss = base + i;
                    sc_open = 1u;
                } else if (sc_open != 0u) {
                    sc_put(sc_ss, base + i - sc_ss);
                    sc_open = 0u;
                }
            }
        }
        sc_prev = (bad >> (hi - 1u)) & 1u;
        p = base + hi;
    }
}

// Scan the part [p, e) of one run, whose first byte has origin offset `o`. Whole words that match
// are skipped while no mismatch is open, since they change nothing but the first match.
void sc_piece(uint p, uint e, uint k, uint o) {
    if (k == uint(O_NEW)) {
        if (o == p) {
            // The new image's own bytes at their own position: nothing can differ.
            if (p < e) {
                sc_good(p);
            }
            return;
        }
        for (; p < e; p++, o++) {
            if (new_byte(o) != new_byte(p)) {
                sc_bad(p);
            } else {
                sc_good(p);
            }
        }
        return;
    }
    if (k == uint(O_ZERO)) {
        sc_bits(p, e, P(P_ZERO_BITS));
        return;
    }
    if (o == p) {
        // The old image at its own place: the bit map says which bytes match.
        sc_bits(p, e, P(P_EQ_BITS));
        return;
    }
    uint old_len = P(P_OLD_LEN);
    while (p < e) {
        if (sc_prev == 0u && p + 4u <= e && o + 4u <= old_len) {
            // Both words aligned, which is the common case, is two loads. Otherwise each word is
            // assembled from two.
            uint a;
            uint b;
            if (((o | p) & 3u) == 0u) {
                a = img[o >> 2u];
                b = img[NB + (p >> 2u)];
            } else {
                a = old_u32(o);
                b = new_u32(p);
            }
            if (a == b) {
                if (sc_ok == NO_POS) {
                    sc_ok = p;
                }
                p += 4u;
                o += 4u;
                continue;
            }
        }
        if (o < old_len && old_byte(o) == new_byte(p)) {
            sc_good(p);
        } else {
            sc_bad(p);
        }
        p++;
        o++;
    }
}

// Scan the bytes [a, b) of candidate `j`'s target. With `emit`, spans that start in the range are
// written from extent index `first_out` of slot `sb`; the caller finishes an open span with
// `sc_finish`. Needs the candidate's runs at `rb`.
void sc_range(uint sb, uint a, uint b, bool emit, uint first_out) {
    sc_n = 0u;
    sc_bytes = 0u;
    sc_ok = NO_POS;
    sc_emit = emit ? 1u : 0u;
    sc_out = first_out;
    sc_sb = sb;
    sc_open = 0u;
    sc_prev = 0u;
    sc_ss = 0u;
    if (a >= b) {
        return;
    }
    if (a > 0u) {
        sc_prev = sc_byte_bad(r_find(a - 1u), a - 1u) ? 1u : 0u;
    }
    uint r = r_find(a);
    uint p = a;
    while (p < b) {
        uint re = min(b, r_start(r) + r_len(r));
        uint k = r_kind(r);
        uint o = k == uint(O_ZERO) ? 0u : r_off(r) + (p - r_start(r));
        sc_piece(p, re, k, o);
        p = re;
        r++;
    }
}

// A span that was still open at the end of the range ends at `end`.
void sc_finish(uint end) {
    if (sc_emit != 0u && sc_open != 0u) {
        sc_put(sc_ss, end - sc_ss);
        sc_open = 0u;
    }
}
