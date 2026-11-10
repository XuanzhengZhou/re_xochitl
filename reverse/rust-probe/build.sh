#!/bin/sh
# 交叉编译 framebuffer 探针
set -e
case "${1:-dynamic}" in
  dynamic)
    # 要画屏：必须动态链接，rm2fb 的 LD_PRELOAD 才能接管
    RUSTFLAGS="-C linker=$HOME/bin/zigcc" \
      cargo build --release --target armv7-unknown-linux-gnueabihf
    cp target/armv7-unknown-linux-gnueabihf/release/fbtest bin/fbtest.armv7.dynamic
    echo "→ bin/fbtest.armv7.dynamic  (设备上: /opt/bin/rm2fb-client ./fbtest)"
    ;;
  static)
    # 只要逻辑/笔迹计算：零依赖，扔上去就能跑
    RUSTFLAGS="-C linker=rust-lld" \
      cargo build --release --target armv7-unknown-linux-musleabihf
    cp target/armv7-unknown-linux-musleabihf/release/fbtest bin/fbtest.armv7.static
    echo "→ bin/fbtest.armv7.static"
    ;;
  *) echo "usage: ./build.sh [dynamic|static]"; exit 1;;
esac
