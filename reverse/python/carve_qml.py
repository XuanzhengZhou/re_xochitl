#!/usr/bin/env python3
"""Carve embedded Qt resource text (QML/JS) out of a stripped binary.

xochitl stores its QML as plain UTF-8 inside the ELF. The resource tree lists
file names in one region and file bodies in another, so we carve printable
runs and classify by content.
"""
import os
import re
import sys

QML_START = re.compile(rb"^\s*(import\s+[A-Za-z]|pragma\s+Singleton|pragma\s+ComponentBehavior|pragma\s+ValueType)")
QML_MARKERS = (b"{", b"}", b"id:", b"property", b"signal", b"function")


def carve(data, min_len=180):
    runs = []
    i, n = 0, len(data)
    while i < n:
        b = data[i]
        printable = b in (0x09, 0x0A, 0x0D) or 0x20 <= b <= 0x7E
        if not printable and b >= 0x80:
            printable = True  # allow UTF-8 (CJK labels)
        if not printable:
            i += 1
            continue
        j = i
        while j < n:
            c = data[j]
            if c in (0x09, 0x0A, 0x0D) or 0x20 <= c <= 0x7E or c >= 0x80:
                j += 1
            else:
                break
        if j - i >= min_len:
            runs.append((i, data[i:j]))
        i = j
    return runs


def score(blob):
    return sum(blob.count(m) for m in QML_MARKERS)


def main(path, outdir):
    data = open(path, "rb").read()
    runs = carve(data)
    kept = []
    for off, blob in runs:
        if b"\x00" in blob:
            blob = blob.split(b"\x00")[0]
        if len(blob) < 180:
            continue
        if score(blob) < 3:
            continue
        try:
            text = blob.decode("utf-8")
        except UnicodeDecodeError:
            continue
        kept.append((off, text))

    os.makedirs(outdir, exist_ok=True)
    index = []
    n_qml = 0
    for k, (off, text) in enumerate(kept):
        kind = "qml" if QML_START.match(text.encode()) else "frag"
        if kind == "qml":
            n_qml += 1
        name = "%s_%04d_%08x.txt" % (kind, k, off)
        with open(os.path.join(outdir, name), "w") as f:
            f.write(text)
        first = text.strip().splitlines()[0][:70] if text.strip() else ""
        index.append("%s\t%6d bytes\t%s" % (name, len(text), first))

    with open(os.path.join(outdir, "INDEX.tsv"), "w") as f:
        f.write("\n".join(index) + "\n")
    print("runs kept: %d (qml-like: %d)" % (len(kept), n_qml))
    print("total text bytes: %d" % sum(len(t) for _, t in kept))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
