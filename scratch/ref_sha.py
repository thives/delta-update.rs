import re, hashlib

src = open('ota-payload/src/sha256.rs').read()

def grab(name):
    m = re.search(r'const %s: \[u32; \d+\] = \[(.*?)\];' % name, src, re.S)
    body = m.group(1)
    return [int(x, 16) for x in re.findall(r'0x([0-9a-fA-F]{8})', body)]

IV = grab('IV')
K = grab('K')
M = 0xFFFFFFFF

def rotr(x, n):
    return ((x >> n) | (x << (32 - n))) & M

# Reference FIPS 180-4 implementation.
def ref_sha(block16):
    w = list(block16)
    for i in range(16, 64):
        s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3)
        s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10)
        w[i] = (w[i - 16] + s0 + w[i - 7] + s1) & M
    a, b, c, d, e, f, g, h = IV
    for i in range(64):
        s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)
        ch = (e & f) ^ ((~e & M) & g)
        t1 = (h + s1 + ch + K[i] + w[i]) & M
        s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)
        maj = (a & b) ^ (a & c) ^ (b & c)
        t2 = (s0 + maj) & M
        h, g, f, e = g, f, e, (d + t1) & M
        d, c, b, a = c, b, a, (t1 + t2) & M
    return [a, b, c, d, e, f, g, h], w

# Build the single padding block for the empty message, exactly as
# SHA-256 requires: 0x80, zeros to byte 56, 64-bit big-endian length 0.
msg = bytearray()
msg.append(0x80)
while len(msg) % 64 != 56:
    msg.append(0)
msg += (0).to_bytes(8, 'big')
print('padded block len', len(msg))
b16 = [int.from_bytes(msg[i * 4:i * 4 + 4], 'big') for i in range(16)]
state, w = ref_sha(b16)
print('ref W:', [hex(x) for x in w])
print('ref state:', [hex(x) for x in state])
print('ref digest:', ''.join('%08x' % x for x in state))
print('openssl digest:', hashlib.sha256(b'').hexdigest())
