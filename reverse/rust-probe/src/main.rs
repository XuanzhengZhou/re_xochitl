use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;


fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn main() {
    let f = OpenOptions::new().read(true).write(true).open("/dev/fb0");
    let f = match f {
        Ok(f) => f,
        Err(e) => {
            println!("open /dev/fb0 failed: {e:#}");
            return;
        }
    };
    let fd = f.as_raw_fd();

    let mut var = [0u8; 160];
    let r = unsafe { libc::ioctl(fd, 0x4600, var.as_mut_ptr()) };
    if r != 0 {
        println!("0x4600 failed: {}", std::io::Error::last_os_error());
        return;
    }
    let (xres, yres, xvirt, yvirt) =
        (u32le(&var, 0), u32le(&var, 4), u32le(&var, 8), u32le(&var, 12));
    let (xo, yo, bpp) = (u32le(&var, 16), u32le(&var, 20), u32le(&var, 24));

    let mut fix = [0u8; 68];
    let r2 = unsafe { libc::ioctl(fd, 0x4602, fix.as_mut_ptr()) };
    let (smem_len, line_len) = if r2 == 0 {
        (u32le(&fix, 20), u32le(&fix, 44))
    } else {
        (0, 0)
    };

    println!("var  : {xres}x{yres} virt {xvirt}x{yvirt} off {xo},{yo} bpp {bpp}");
    println!("fix  : line_length {line_len} smem_len {smem_len} (expect {} bytes)", line_len as u64 * yres as u64);

    let frame = line_len as usize * yres as usize;
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            frame,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        println!("mmap failed: {}", std::io::Error::last_os_error());
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(p as *const u8, frame) };
    let nonzero = slice.iter().filter(|b| **b != 0).count();
    println!("mmap : ok, {frame} bytes, {nonzero} non-zero bytes");
    unsafe { libc::munmap(p, frame) };
    println!("OK: Rust 已拿到可读写的帧缓冲");
}
