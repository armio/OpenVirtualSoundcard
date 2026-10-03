//! The checks the plug-in runs inside the driver host. Each result is logged
//! as one `ovprobe:` line (unified log) and collected into a report that is
//! sent to the daemon over XPC, when XPC works, and also written to /tmp.

use std::ffi::{CString, c_void};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ovprobe_shim::layout::*;
use ovprobe_shim::*;

static REPORT: Mutex<String> = Mutex::new(String::new());

pub fn log(msg: &str) {
    let line = CString::new(format!("ovprobe: {msg}").replace('\0', " ")).unwrap_or_default();
    unsafe { libc::syslog(libc::LOG_NOTICE, c"%s".as_ptr(), line.as_ptr()) };
}

fn record(name: &str, ok: bool, detail: impl AsRef<str>) {
    let line = format!("{:4} {name}: {}", if ok { "PASS" } else { "FAIL" }, detail.as_ref());
    log(&line);
    let mut r = REPORT.lock().unwrap();
    r.push_str(&line);
    r.push('\n');
}

fn info(name: &str, detail: impl AsRef<str>) {
    let line = format!("INFO {name}: {}", detail.as_ref());
    log(&line);
    let mut r = REPORT.lock().unwrap();
    r.push_str(&line);
    r.push('\n');
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

pub fn run() {
    let started = Instant::now();
    environment();
    sandbox_checks();
    mach_lookup();
    xpc();
    posix_shm();
    files();
    sockets();
    realtime();
    info("elapsed", format!("{} ms", started.elapsed().as_millis()));
    finish();
}

fn environment() {
    info("process", format!("{} pid {}", progname(), unsafe { libc::getpid() }));
    info("path", pid_path(unsafe { libc::getpid() }));
    info(
        "ids",
        format!(
            "uid {} euid {} gid {} egid {}",
            unsafe { libc::getuid() },
            unsafe { libc::geteuid() },
            unsafe { libc::getgid() },
            unsafe { libc::getegid() }
        ),
    );
    info(
        "os",
        format!(
            "{} build {} arch {}",
            sysctl_string("kern.osproductversion").unwrap_or_default(),
            sysctl_string("kern.osversion").unwrap_or_default(),
            std::env::consts::ARCH
        ),
    );
    info("translated", format!("{:?}", sysctl_i32("sysctl.proc_translated")));
    let tb = timebase();
    info("timebase", format!("{}/{}", tb.numer, tb.denom));
    let raw = unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) };
    let mach = host_ns();
    info(
        "clock",
        format!("CLOCK_UPTIME_RAW - mach_absolute_time = {} ns", raw as i64 - mach as i64),
    );
}

fn sandbox_checks() {
    info("sandboxed", format!("{}", unsafe { ovshim_sandboxed() }));
    for name in [SERVICE, UNLISTED_SERVICE] {
        let (op, n) = (cstr("mach-lookup"), cstr(name));
        let r = unsafe { ovshim_sandbox_allows_name(op.as_ptr(), n.as_ptr()) };
        info(&format!("sandbox_check mach-lookup {name}"), verdict(r));
    }
    for (op, path) in [
        ("file-write-create", "/tmp/ovprobe-x"),
        ("file-read-data", "/tmp/ovprobe-daemon.txt"),
        ("file-read-data", "/Library/Application Support/OpenVirtualSoundcard/ovprobe-daemon.txt"),
    ] {
        let (o, p) = (cstr(op), cstr(path));
        let r = unsafe { ovshim_sandbox_allows_path(o.as_ptr(), p.as_ptr()) };
        info(&format!("sandbox_check {op} {path}"), verdict(r));
    }
}

fn verdict(r: i32) -> &'static str {
    match r {
        0 => "allowed",
        1 => "denied",
        _ => "unknown",
    }
}

fn mach_lookup() {
    for name in [SERVICE, UNLISTED_SERVICE] {
        let n = cstr(name);
        let kr = unsafe { ovshim_bootstrap_look_up(n.as_ptr()) };
        record(&format!("bootstrap_look_up {name}"), kr == 0, format!("kern_return {kr:#x}"));
    }
}

/// Events on the client connection are only logged; requests use the
/// synchronous reply call.
unsafe extern "C" fn client_event(_ctx: *mut c_void, _conn: xpc_connection_t, event: xpc_object_t) {
    let kind = unsafe { ovshim_kind(event) };
    if kind != DICTIONARY {
        log(&format!("xpc client event kind {kind}: {}", unsafe { describe(event) }));
    }
}

static CONNECTION: Mutex<usize> = Mutex::new(0);

fn xpc() {
    let service = cstr(SERVICE);
    let conn = unsafe { ovshim_connect(service.as_ptr(), client_event, ptr::null_mut()) };
    if conn.is_null() {
        record("xpc connect", false, "xpc_connection_create_mach_service returned NULL");
        return;
    }
    *CONNECTION.lock().unwrap() = conn as usize;

    // A plug-in-owned anonymous shared region, handed over with xpc_shmem.
    let region = unsafe {
        libc::mmap(
            ptr::null_mut(),
            REGION_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_SHARED,
            -1,
            0,
        )
    };
    if region == libc::MAP_FAILED {
        record("xpc region mmap", false, format!("{}", errno()));
        return;
    }
    let base = region as *mut u8;
    unsafe { ptr::copy_nonoverlapping(PLUGIN_MAGIC.as_ptr(), base.add(OFF_OWNER_MAGIC), 16) };
    let shmem = unsafe { xpc_shmem_create(region, REGION_SIZE) };
    record("xpc_shmem_create", !shmem.is_null(), format!("{REGION_SIZE} bytes"));
    if shmem.is_null() {
        return;
    }

    let hello = Dict::new();
    hello.set_str("op", "hello").set_i64("pid", unsafe { libc::getpid() } as i64);
    unsafe {
        hello.set_value("shm", shmem);
        xpc_release(shmem);
    }
    let t0 = Instant::now();
    let reply = unsafe { xpc_connection_send_message_with_reply_sync(conn, hello.0) };
    let rtt = t0.elapsed();
    let kind = unsafe { ovshim_kind(reply) };
    if kind != DICTIONARY {
        record("xpc hello", false, format!("reply kind {kind}: {}", unsafe { describe(reply) }));
        unsafe { xpc_release(reply) };
        return;
    }
    let reply = Dict(reply);
    record("xpc hello", true, format!("round trip {} µs", rtt.as_micros()));
    let mapped = unsafe { get_u64(reply.0, "mapped") };
    let saw = unsafe { get_bool(reply.0, "saw_plugin_magic") };
    record(
        "xpc_shmem plug-in region mapped by daemon",
        mapped as usize >= REGION_SIZE && saw,
        format!("mapped {mapped} bytes, saw magic {saw}"),
    );
    info("daemon sees peer", unsafe { get_str(reply.0, "peer") }.unwrap_or_default());

    // The daemon wrote its magic into our region and keeps a counter and its
    // host time there.
    let peer_magic = unsafe { std::slice::from_raw_parts(base.add(OFF_PEER_MAGIC), 16) };
    record(
        "xpc_shmem daemon writes visible to plug-in",
        peer_magic == DAEMON_MAGIC,
        format!("{:?}", String::from_utf8_lossy(peer_magic)),
    );
    live_counter("xpc_shmem plug-in region live", base);

    // A daemon-owned region sent back in the reply.
    let dshm = unsafe { get_value(reply.0, "dshm") };
    if dshm.is_null() {
        record("xpc_shmem daemon region", false, "no dshm in reply");
        return;
    }
    let mut addr: *mut c_void = ptr::null_mut();
    let size = unsafe { xpc_shmem_map(dshm, &mut addr) };
    if size == 0 || addr.is_null() {
        record("xpc_shmem_map daemon region", false, format!("size {size}"));
        return;
    }
    let dbase = addr as *mut u8;
    let owner = unsafe { std::slice::from_raw_parts(dbase.add(OFF_OWNER_MAGIC), 16) };
    record(
        "xpc_shmem_map daemon region",
        owner == DAEMON_MAGIC,
        format!("{size} bytes, magic {:?}", String::from_utf8_lossy(owner)),
    );
    unsafe { ptr::copy_nonoverlapping(PLUGIN_MAGIC.as_ptr(), dbase.add(OFF_PEER_MAGIC), 16) };
    let check = Dict::new();
    check.set_str("op", "check_dshm");
    let r = unsafe { xpc_connection_send_message_with_reply_sync(conn, check.0) };
    let ok = unsafe { ovshim_kind(r) } == DICTIONARY && unsafe { get_bool(r, "ok") };
    unsafe { xpc_release(r) };
    record("xpc_shmem plug-in writes to daemon region visible", ok, "");
    live_counter("xpc_shmem daemon region live", dbase);
    regions(conn);
}

/// Sizing for the OpenVirtualSoundcard layout: 64 MiB and 32 MiB daemon regions, mlock,
/// page-touch cost, and replacing a live xpc mapping with a private
/// placeholder (the driver's retire protocol).
fn regions(conn: xpc_connection_t) {
    let req = Dict::new();
    req.set_str("op", "regions");
    let r = unsafe { xpc_connection_send_message_with_reply_sync(conn, req.0) };
    if unsafe { ovshim_kind(r) } != DICTIONARY {
        record("regions request", false, unsafe { describe(r) });
        unsafe { xpc_release(r) };
        return;
    }
    let reply = Dict(r);
    for (name, size) in BIG_REGIONS {
        let shm = unsafe { get_value(reply.0, name) };
        if shm.is_null() {
            record(&format!("{name} ({size} B) received"), false, "missing from reply");
            continue;
        }
        let mut addr: *mut c_void = ptr::null_mut();
        let t0 = Instant::now();
        let mapped = unsafe { xpc_shmem_map(shm, &mut addr) };
        let map_us = t0.elapsed().as_micros();
        if mapped < size || addr.is_null() {
            record(&format!("{name} ({size} B) xpc_shmem_map"), false, format!("mapped {mapped}"));
            continue;
        }
        record(
            &format!("{name} ({size} B) xpc_shmem_map"),
            true,
            format!("{mapped} B in {map_us} µs"),
        );
        let base = addr as *mut u64;
        if name != "r1" {
            let n = size / STRIDE;
            let mut good = 0;
            for i in 0..n / 2 {
                let v = stride_pattern(true, size, i);
                let (a, b) = unsafe {
                    (
                        ptr::read_volatile(base.add(i * STRIDE / 8)),
                        ptr::read_volatile(base.add(((i + 1) * STRIDE - 8) / 8)),
                    )
                };
                good += (a == v && b == v) as usize;
            }
            record(
                &format!("{name} daemon strides visible"),
                good == n / 2,
                format!("{good} of {}", n / 2),
            );
            for i in n / 2..n {
                let v = stride_pattern(false, size, i);
                unsafe {
                    ptr::write_volatile(base.add(i * STRIDE / 8), v);
                    ptr::write_volatile(base.add(((i + 1) * STRIDE - 8) / 8), v);
                }
            }
            if name == "r64" {
                mlock_checks(addr, size);
                touch_timing(addr as *mut u8, size);
                live_counter("r64 live (heartbeat)", addr as *mut u8);
            }
        } else {
            placeholder(addr, size);
        }
    }
    let check = Dict::new();
    check.set_str("op", "check_regions");
    let r = unsafe { xpc_connection_send_message_with_reply_sync(conn, check.0) };
    if unsafe { ovshim_kind(r) } != DICTIONARY {
        record("check_regions", false, unsafe { describe(r) });
        unsafe { xpc_release(r) };
        return;
    }
    let r = Dict(r);
    for (name, _) in BIG_REGIONS {
        if name != "r1" {
            let (good, want) = unsafe {
                (get_u64(r.0, &format!("{name}_good")), get_u64(r.0, &format!("{name}_want")))
            };
            record(
                &format!("{name} plug-in strides visible to daemon"),
                want > 0 && good == want,
                format!("{good} of {want}"),
            );
        } else {
            let (intact, pages) = unsafe {
                (get_u64(r.0, &format!("{name}_intact")), get_u64(r.0, &format!("{name}_pages")))
            };
            record(
                &format!("{name} daemon copy intact after plug-in placeholder"),
                pages > 0 && intact == pages,
                format!("{intact} of {pages} pages"),
            );
        }
    }
}

fn mlock_checks(addr: *mut c_void, size: usize) {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) };
    info("RLIMIT_MEMLOCK", format!("cur {} max {}", lim.rlim_cur, lim.rlim_max));
    for (label, len) in [("2 MiB", 2 << 20), ("whole region", size)] {
        let r = unsafe { libc::mlock(addr, len) };
        let e = errno();
        info(&format!("mlock {label}"), if r == 0 { "ok".into() } else { format!("{e}") });
        if r == 0 {
            unsafe { libc::munlock(addr, len) };
        }
    }
}

fn touch_timing(base: *mut u8, size: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    let start = 16 << 20;
    let len = (16 << 20).min(size - start);
    let t0 = Instant::now();
    for off in (start..start + len).step_by(PAGE) {
        let a = unsafe { &*(base.add(off) as *const AtomicU64) };
        a.fetch_add(0, Ordering::Relaxed);
    }
    let first = t0.elapsed().as_micros();
    let t1 = Instant::now();
    for off in (start..start + len).step_by(PAGE) {
        let a = unsafe { &*(base.add(off) as *const AtomicU64) };
        a.fetch_add(0, Ordering::Relaxed);
    }
    info(
        "touch 16 MiB, one fetch_add per 16 KiB page",
        format!("first {first} µs, again {} µs", t1.elapsed().as_micros()),
    );
}

/// Replaces our mapping of a daemon region with an anonymous private one at
/// the same address, then unmaps it.
fn placeholder(addr: *mut c_void, size: usize) {
    let base = addr as *mut u64;
    let pages = size / PAGE;
    let before = (0..pages)
        .filter(
            |&p| unsafe { ptr::read_volatile(base.add(p * PAGE / 8)) } == 0xD1D1_0000 | p as u64,
        )
        .count();
    record("r1 daemon pattern visible", before == pages, format!("{before} of {pages} pages"));
    let p = unsafe {
        libc::mmap(
            addr,
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED | libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if p != addr {
        record(
            "MAP_FIXED placeholder over xpc mapping",
            false,
            format!("mmap returned {p:?}: {}", errno()),
        );
        return;
    }
    let zeros =
        (0..pages).filter(|&q| unsafe { ptr::read_volatile(base.add(q * PAGE / 8)) } == 0).count();
    for q in 0..pages {
        unsafe { ptr::write_volatile(base.add(q * PAGE / 8), 0xBEEF) };
    }
    let r = unsafe { libc::munmap(addr, size) };
    record(
        "MAP_FIXED placeholder over xpc mapping",
        zeros == pages && r == 0,
        format!(
            "{zeros} of {pages} pages read zero; munmap {}",
            if r == 0 { "ok".into() } else { errno().to_string() }
        ),
    );
}

/// The daemon increments a counter and stores mach_absolute_time at fixed
/// offsets every millisecond; check both move and the host time matches ours.
fn live_counter(name: &str, base: *mut u8) {
    let read = |off: usize| unsafe { ptr::read_volatile(base.add(off) as *const u64) };
    let c0 = read(OFF_COUNTER);
    std::thread::sleep(Duration::from_millis(50));
    let c1 = read(OFF_COUNTER);
    let theirs = read(OFF_HOST_TICKS);
    let ours = unsafe { mach_absolute_time() };
    let skew_us = (host_ticks_to_ns(ours) as i64 - host_ticks_to_ns(theirs) as i64) / 1000;
    record(name, c1 > c0, format!("counter {c0} -> {c1}, host time skew {skew_us} µs"));
}

fn posix_shm() {
    for (name, mode, _) in PSHM {
        for (label, flags, prot) in [
            ("O_RDWR", libc::O_RDWR, libc::PROT_READ | libc::PROT_WRITE),
            ("O_RDONLY", libc::O_RDONLY, libc::PROT_READ),
        ] {
            let test = format!("pshm open {name} ({mode:o}) {label}");
            let n = cstr(name);
            let fd = unsafe { libc::shm_open(n.as_ptr(), flags) };
            if fd < 0 {
                record(&test, false, format!("{}", errno()));
                continue;
            }
            let p =
                unsafe { libc::mmap(ptr::null_mut(), PSHM_SIZE, prot, libc::MAP_SHARED, fd, 0) };
            unsafe { libc::close(fd) };
            if p == libc::MAP_FAILED {
                record(&test, false, format!("mmap: {}", errno()));
                continue;
            }
            let base = p as *mut u8;
            let magic = unsafe { std::slice::from_raw_parts(base, 16) }.to_vec();
            let mut detail = format!("magic {:?}", String::from_utf8_lossy(&magic));
            if flags == libc::O_RDWR {
                unsafe { ptr::write_volatile(base.add(OFF_PEER_MAGIC), b'P') };
                detail.push_str(", wrote 1 byte");
            }
            if name == PSHM[0].0 {
                let c0 = unsafe { ptr::read_volatile(base.add(OFF_COUNTER) as *const u64) };
                std::thread::sleep(Duration::from_millis(20));
                let c1 = unsafe { ptr::read_volatile(base.add(OFF_COUNTER) as *const u64) };
                let _ = write!(detail, ", counter {c0} -> {c1}");
            }
            record(&test, magic.as_slice() == DAEMON_MAGIC, detail);
            unsafe { libc::munmap(p, PSHM_SIZE) };
        }
    }
    let n = cstr(PSHM_PLUGIN);
    unsafe { libc::shm_unlink(n.as_ptr()) };
    let fd =
        unsafe { libc::shm_open(n.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
    if fd < 0 {
        record("pshm create by plug-in", false, format!("{}", errno()));
    } else {
        let t = unsafe { libc::ftruncate(fd, PSHM_SIZE as libc::off_t) };
        record(
            "pshm create by plug-in",
            t == 0,
            if t == 0 { "created + ftruncate".into() } else { format!("ftruncate: {}", errno()) },
        );
        unsafe {
            libc::close(fd);
            libc::shm_unlink(n.as_ptr());
        }
    }
}

fn files() {
    for path in DAEMON_FILES {
        match std::fs::read_to_string(path) {
            Ok(s) => record(&format!("read {path}"), true, s.trim()),
            Err(e) => record(&format!("read {path}"), false, e.to_string()),
        }
    }
    let mut dirs = vec!["/tmp".to_string(), "/private/var/tmp".to_string()];
    for (label, key) in
        [("temp", libc::_CS_DARWIN_USER_TEMP_DIR), ("cache", libc::_CS_DARWIN_USER_CACHE_DIR)]
    {
        let mut buf = vec![0u8; 1024];
        let n = unsafe { libc::confstr(key, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 && n <= buf.len() {
            buf.truncate(n - 1);
            let dir = String::from_utf8_lossy(&buf).trim_end_matches('/').to_string();
            info(&format!("confstr {label}"), &dir);
            dirs.push(dir);
        } else {
            info(&format!("confstr {label}"), format!("failed: {}", errno()));
        }
    }
    for dir in dirs {
        let path = format!("{dir}/ovprobe-plugin.txt");
        let r = std::fs::write(&path, "written by the plug-in\n");
        record(
            &format!("create {path}"),
            r.is_ok(),
            r.err().map(|e| e.to_string()).unwrap_or_default(),
        );
        let _ = std::fs::remove_file(&path);
    }
    // A shared mapping of a daemon-created file.
    let path = "/tmp/ovprobe-daemon.map";
    match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => {
            use std::os::fd::AsRawFd;
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                record("mmap RW /tmp file", false, format!("{}", errno()));
            } else {
                record("mmap RW /tmp file", true, "");
                unsafe { libc::munmap(p, 4096) };
            }
        }
        Err(e) => record("mmap RW /tmp file", false, format!("open: {e}")),
    }
}

fn sockets() {
    for path in UNIX_SOCKETS {
        let r = UnixStream::connect(path).and_then(|mut s| {
            s.set_read_timeout(Some(Duration::from_secs(1)))?;
            s.write_all(b"ping")?;
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf)?;
            Ok(buf)
        });
        record(&format!("AF_UNIX {path}"), matches!(r, Ok(b) if &b == b"pong"), fmt_result(&r));
    }
    let udp = UdpSocket::bind("127.0.0.1:0").and_then(|s| {
        s.set_read_timeout(Some(Duration::from_secs(1)))?;
        s.send_to(b"ping", ("127.0.0.1", UDP_PORT))?;
        let mut buf = [0u8; 4];
        let (n, _) = s.recv_from(&mut buf)?;
        Ok(buf[..n].to_vec())
    });
    record("UDP 127.0.0.1 echo", matches!(&udp, Ok(b) if b == b"pong"), fmt_result(&udp));
    let tcp =
        TcpStream::connect_timeout(&([127, 0, 0, 1], TCP_PORT).into(), Duration::from_secs(1))
            .and_then(|mut s| {
                s.set_read_timeout(Some(Duration::from_secs(1)))?;
                s.write_all(b"ping")?;
                let mut buf = [0u8; 4];
                s.read_exact(&mut buf)?;
                Ok(buf)
            });
    record("TCP 127.0.0.1 echo", matches!(tcp, Ok(b) if &b == b"pong"), fmt_result(&tcp));
    let mc = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.join_multicast_v4(&[224, 0, 0, 231].into(), &[127, 0, 0, 1].into()));
    record("UDP multicast join on lo0", mc.is_ok(), fmt_result(&mc));
}

fn fmt_result<T: std::fmt::Debug>(r: &std::io::Result<T>) -> String {
    match r {
        Ok(v) => format!("{v:?}"),
        Err(e) => e.to_string(),
    }
}

fn realtime() {
    // 1 ms period, 0.5 ms computation, 1 ms constraint.
    let kr = unsafe { ovshim_make_realtime(1_000_000, 500_000, 1_000_000) };
    record("THREAD_TIME_CONSTRAINT_POLICY", kr == 0, format!("kern_return {kr:#x}"));
}

fn finish() {
    let report = REPORT.lock().unwrap().clone();
    let tmp = std::fs::write("/tmp/ovprobe-plugin-report.txt", &report);
    log(&format!("report written to /tmp: {}", tmp.is_ok()));
    let conn = *CONNECTION.lock().unwrap() as xpc_connection_t;
    if conn.is_null() {
        log("no XPC connection; report only in the log");
        return;
    }
    let msg = Dict::new();
    msg.set_str("op", "report").set_str("text", &report);
    let r = unsafe { xpc_connection_send_message_with_reply_sync(conn, msg.0) };
    log(&format!("report sent over XPC: reply kind {}", unsafe { ovshim_kind(r) }));
    unsafe { xpc_release(r) };
}
