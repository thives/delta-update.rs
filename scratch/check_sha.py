import re, sys, math

src = open('ota-payload/src/sha256.rs').read()

# Pull the IV and K arrays out of the Rust source.
def grab(name):
    m = re.search(r'const %s: \[u32; \d+\] = \[(.*?)\];' % name, src, re.S)
    body = m.group(1)
    return [int(x, 16) for x in re.findall(r'0x([0-9a-fA-F]{8})', body)]

iv = grab('IV')
k = grab('K')

def primes(n):
    out, c = [], 2
    while len(out) < n:
        if all(c % p for p in out):
            out.append(c)
        c += 1
    return out

def isqrt_scaled(x, s):
    # floor(x**(1/2) * 2**s)
    return math.isqrt(x << (2 * s))

def icbrt_scaled(x, s):
    # floor(x**(1/3) * 2**s) by binary search
    lo, hi = 0, 1 << ((x.bit_length() + 3 * s + 2) // 3 + 1)
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if mid ** 3 <= (x << (3 * s)):
            lo = mid
        else:
            hi = mid - 1
    return lo

S = 200
p = primes(64)
iv_ref = []
k_ref = []
for i in range(8):
    v = isqrt_scaled(p[i], S)
    whole = v >> S
    frac32 = ((v - (whole << S)) * (1 << 32)) >> S
    iv_ref.append(frac32)
for i in range(64):
    v = icbrt_scaled(p[i], S)
    whole = v >> S
    frac32 = ((v - (whole << S)) * (1 << 32)) >> S
    k_ref.append(frac32)

print('IV count', len(iv), 'K count', len(k))
print('IV match:', iv == iv_ref)
for i in range(min(len(iv), 8)):
    if iv[i] != iv_ref[i]:
        print('IV diff at', i, hex(iv[i]), hex(iv_ref[i]))
bad = [i for i in range(64) if k[i] != k_ref[i]]
print('K match:', not bad)
for i in bad:
    print('K diff at', i, hex(k[i]), hex(k_ref[i]))
