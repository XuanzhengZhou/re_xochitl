#!/usr/bin/env python3
"""reMarkable 2 的 u-boot 环境变量工具（ENV_IS_IN_FAT，文件 /var/lib/uboot/uboot.env）。

用法:
  ./rm2_envtool.py show  uboot.env
  ./rm2_envtool.py set   uboot.env active_partition=4
  ./rm2_envtool.py check uboot.env
"""
import struct, sys, zlib

TOTAL = 8192          # fw_env.config: 0x2000
CRC_LEN = 4
DATA_LEN = TOTAL - CRC_LEN


def load(path):
    raw = open(path, 'rb').read()
    if len(raw) != TOTAL:
        raise SystemExit(f'预期 {TOTAL} 字节，实际 {len(raw)}')
    stored, = struct.unpack('<I', raw[:4])
    data = bytearray(raw[CRC_LEN:])
    calc = zlib.crc32(bytes(data)) & 0xffffffff
    return stored, data, calc


def parse(data):
    out = {}
    for item in bytes(data).split(b'\x00'):
        if b'=' in item:
            k, v = item.split(b'=', 1)
            out[k.decode()] = v.decode('latin1')
    return out


def dump(data, vars_):
    blob = bytearray()
    for k, v in vars_.items():
        blob += f'{k}={v}'.encode('latin1') + b'\x00'
    blob += b'\x00'
    if len(blob) > DATA_LEN:
        raise SystemExit('环境变量过大')
    data[:] = blob + b'\x00' * (DATA_LEN - len(blob))


def save(path, data):
    crc = zlib.crc32(bytes(data)) & 0xffffffff
    with open(path, 'wb') as f:
        f.write(struct.pack('<I', crc) + bytes(data))
    print(f'已写入 {path}, CRC=0x{crc:08x}')


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    cmd, path = sys.argv[1], sys.argv[2]
    stored, data, calc = load(path)
    vars_ = parse(data)

    if cmd == 'show':
        print(f'CRC stored=0x{stored:08x} calc=0x{calc:08x} '
              f'({"OK" if stored == calc else "MISMATCH"})')
        for k in sorted(vars_):
            print(f'{k}={vars_[k]}')
    elif cmd == 'check':
        print('CRC OK' if stored == calc else 'CRC MISMATCH')
        return 0 if stored == calc else 1
    elif cmd == 'set':
        for pair in sys.argv[3:]:
            k, v = pair.split('=', 1)
            print(f'{k}: {vars_.get(k)} -> {v}')
            vars_[k] = v
        dump(data, vars_)
        save(path, data)
    else:
        raise SystemExit(__doc__)


if __name__ == '__main__':
    sys.exit(main() or 0)
