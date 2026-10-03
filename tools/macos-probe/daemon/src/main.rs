//! Test LaunchDaemon for the sandbox probe. It offers every channel the
//! plug-in tries (XPC, POSIX shared memory, files, sockets) and writes the
//! plug-in's report to /var/log/ovprobe-report.txt.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ovprobe-daemon only runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    imp::main()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::io::{Read, Write};
    use std::net::{TcpListener, UdpSocket};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::ptr;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use ovprobe_shim::layout::*;
    use ovprobe_shim::*;

    macro_rules! say {
        ($($t:tt)*) => {{
            println!("[ovprobe-daemon {:>8.3}] {}", uptime(), format!($($t)*));
        }};
    }

    fn uptime() -> f64 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    /// Address of the daemon-owned region shared through xpc_shmem.
    static DAEMON_REGION: Mutex<usize> = Mutex::new(0);
    /// (name, address, size) of the sizing regions.
    static BIG: Mutex<Vec<(&'static str, usize, usize)>> = Mutex::new(Vec::new());

    pub fn main() {
        uptime();
        say!(
            "pid {} uid {} euid {} on {}",
            unsafe { libc::getpid() },
            unsafe { libc::getuid() },
            unsafe { libc::geteuid() },
            sysctl_string("kern.osproductversion").unwrap_or_default()
        );
        files();
        posix_shm();
        sockets();
        for name in [SERVICE, UNLISTED_SERVICE] {
            let n = cstr(name);
            let l = unsafe { ovshim_listen(n.as_ptr(), on_event, ptr::null_mut()) };
            say!("listening on Mach service {name}: {}", !l.is_null());
        }
        // Handlers run on libdispatch threads; the main thread only waits.
        std::thread::sleep(Duration::from_secs(3600));
    }

    fn files() {
        let _ = std::fs::create_dir_all("/Library/Application Support/OpenVirtualSoundcard");
        for path in DAEMON_FILES {
            let r = std::fs::write(path, "written by the daemon\n").and_then(|_| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
            });
            say!("file {path}: {r:?}");
        }
        let map = "/tmp/ovprobe-daemon.map";
        let r = std::fs::write(map, vec![0u8; 4096])
            .and_then(|_| std::fs::set_permissions(map, std::fs::Permissions::from_mode(0o666)));
        say!("file {map}: {r:?}");
    }

    fn posix_shm() {
        let mut live = Vec::new();
        for (name, mode, coreaudiod_group) in PSHM {
            let n = cstr(name);
            unsafe { libc::shm_unlink(n.as_ptr()) };
            if coreaudiod_group {
                unsafe { libc::setegid(COREAUDIOD_GID) };
            }
            let fd = unsafe {
                libc::shm_open(
                    n.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    mode as libc::c_uint,
                )
            };
            let err = std::io::Error::last_os_error();
            if coreaudiod_group {
                unsafe { libc::setegid(0) };
            }
            if fd < 0 {
                say!("pshm {name}: shm_open: {err}");
                continue;
            }
            if unsafe { libc::ftruncate(fd, PSHM_SIZE as libc::off_t) } != 0 {
                say!("pshm {name}: ftruncate: {}", std::io::Error::last_os_error());
            }
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            unsafe { libc::fstat(fd, &mut st) };
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    PSHM_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                say!("pshm {name}: mmap: {}", std::io::Error::last_os_error());
                continue;
            }
            unsafe { ptr::copy_nonoverlapping(DAEMON_MAGIC.as_ptr(), p as *mut u8, 16) };
            say!(
                "pshm {name}: mode {:o} uid {} gid {} size {}",
                st.st_mode & 0o7777,
                st.st_uid,
                st.st_gid,
                st.st_size
            );
            live.push(p as usize);
        }
        spawn_ticker(live, Duration::from_secs(3600));
    }

    /// Increments the counter and stores mach_absolute_time every
    /// millisecond in each region, for `duration`.
    fn spawn_ticker(regions: Vec<usize>, duration: Duration) {
        std::thread::spawn(move || {
            let end = Instant::now() + duration;
            let mut n = 0u64;
            while Instant::now() < end {
                n += 1;
                let ticks = unsafe { mach_absolute_time() };
                for &r in &regions {
                    let base = r as *mut u8;
                    unsafe {
                        ptr::write_volatile(base.add(OFF_COUNTER) as *mut u64, n);
                        ptr::write_volatile(base.add(OFF_HOST_TICKS) as *mut u64, ticks);
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }

    fn sockets() {
        match UdpSocket::bind(("127.0.0.1", UDP_PORT)) {
            Ok(s) => {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 64];
                    while let Ok((n, from)) = s.recv_from(&mut buf) {
                        say!("udp {} bytes from {from}", n);
                        let _ = s.send_to(b"pong", from);
                    }
                });
            }
            Err(e) => say!("udp bind: {e}"),
        }
        match TcpListener::bind(("127.0.0.1", TCP_PORT)) {
            Ok(l) => {
                std::thread::spawn(move || {
                    for s in l.incoming().flatten() {
                        say!("tcp connection from {:?}", s.peer_addr());
                        echo(s);
                    }
                });
            }
            Err(e) => say!("tcp bind: {e}"),
        }
        for path in UNIX_SOCKETS {
            let _ = std::fs::remove_file(path);
            match UnixListener::bind(path) {
                Ok(l) => {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
                    std::thread::spawn(move || {
                        for s in l.incoming().flatten() {
                            say!("unix connection on {path}");
                            echo(s);
                        }
                    });
                }
                Err(e) => say!("unix bind {path}: {e}"),
            }
        }
    }

    fn echo<S: Read + Write>(mut s: S) {
        let mut buf = [0u8; 4];
        if s.read_exact(&mut buf).is_ok() {
            let _ = s.write_all(b"pong");
        }
    }

    unsafe extern "C" fn on_event(_ctx: *mut c_void, conn: xpc_connection_t, event: xpc_object_t) {
        let kind = unsafe { ovshim_kind(event) };
        if kind != DICTIONARY {
            say!("xpc event kind {kind}: {}", unsafe { describe(event) });
            return;
        }
        let op = unsafe { get_str(event, "op") }.unwrap_or_default();
        let pid = unsafe { xpc_connection_get_pid(conn) };
        let peer = format!(
            "pid {pid} euid {} path {}",
            unsafe { xpc_connection_get_euid(conn) },
            pid_path(pid)
        );
        say!("xpc {op:?} from {peer}");
        let Some(reply) = (unsafe { Dict::reply_to(event) }) else {
            say!("message expects no reply");
            return;
        };
        reply.set_str("peer", &peer);
        match op.as_str() {
            "hello" => unsafe { hello(event, &reply) },
            "check_dshm" => {
                let r = *DAEMON_REGION.lock().unwrap();
                let ok = r != 0 && {
                    let seen = unsafe {
                        std::slice::from_raw_parts((r as *const u8).add(OFF_PEER_MAGIC), 16)
                    };
                    seen == PLUGIN_MAGIC
                };
                say!("check_dshm: plug-in write visible: {ok}");
                reply.set_bool("ok", ok);
            }
            "regions" => unsafe { regions(&reply) },
            "check_regions" => check_regions(&reply),
            "report" => {
                let text = unsafe { get_str(event, "text") }.unwrap_or_default();
                let r = std::fs::write(REPORT_PATH, &text);
                say!("report ({} bytes) -> {REPORT_PATH}: {r:?}\n{text}", text.len());
                reply.set_bool("ok", r.is_ok());
            }
            other => {
                say!("unknown op {other:?}");
                reply.set_bool("ok", false);
            }
        }
        unsafe { xpc_connection_send_message(conn, reply.0) };
    }

    unsafe fn hello(event: xpc_object_t, reply: &Dict) {
        let shm = unsafe { get_value(event, "shm") };
        let mut addr: *mut c_void = ptr::null_mut();
        let size = if shm.is_null() { 0 } else { unsafe { xpc_shmem_map(shm, &mut addr) } };
        let mut saw = false;
        if size > 0 && !addr.is_null() {
            let base = addr as *mut u8;
            saw = unsafe { std::slice::from_raw_parts(base.add(OFF_OWNER_MAGIC), 16) }
                == PLUGIN_MAGIC;
            unsafe {
                ptr::copy_nonoverlapping(DAEMON_MAGIC.as_ptr(), base.add(OFF_PEER_MAGIC), 16)
            };
            spawn_ticker(vec![addr as usize], Duration::from_secs(120));
        }
        say!("hello: mapped plug-in region {size} bytes, magic seen {saw}");
        reply.set_u64("mapped", size as u64).set_bool("saw_plugin_magic", saw);

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
            say!("daemon region mmap: {}", std::io::Error::last_os_error());
            return;
        }
        unsafe { ptr::copy_nonoverlapping(DAEMON_MAGIC.as_ptr(), region as *mut u8, 16) };
        *DAEMON_REGION.lock().unwrap() = region as usize;
        spawn_ticker(vec![region as usize], Duration::from_secs(120));
        let dshm = unsafe { xpc_shmem_create(region, REGION_SIZE) };
        if !dshm.is_null() {
            unsafe {
                reply.set_value("dshm", dshm);
                xpc_release(dshm);
            }
        }
    }

    /// Creates the sizing regions exactly as OpenVirtualSoundcard will: mmap, then
    /// xpc_shmem_create straight away. The lower half of r64/r32 gets the
    /// daemon's stride pattern; r1 gets a pattern on every page.
    unsafe fn regions(reply: &Dict) {
        let mut big = BIG.lock().unwrap();
        for (name, size) in BIG_REGIONS {
            let t0 = Instant::now();
            let region = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | libc::MAP_SHARED,
                    -1,
                    0,
                )
            };
            if region == libc::MAP_FAILED {
                say!("{name}: mmap {size}: {}", std::io::Error::last_os_error());
                continue;
            }
            let shm = unsafe { xpc_shmem_create(region, size) };
            let created = t0.elapsed();
            if shm.is_null() {
                say!("{name}: xpc_shmem_create({size}) returned NULL");
                continue;
            }
            let base = region as *mut u64;
            if name != "r1" {
                for i in 0..size / STRIDE / 2 {
                    let v = stride_pattern(true, size, i);
                    unsafe {
                        ptr::write_volatile(base.add(i * STRIDE / 8), v);
                        ptr::write_volatile(base.add(((i + 1) * STRIDE - 8) / 8), v);
                    }
                }
            } else {
                for p in 0..size / PAGE {
                    unsafe { ptr::write_volatile(base.add(p * PAGE / 8), 0xD1D1_0000 | p as u64) };
                }
            }
            if name == "r64" {
                spawn_ticker(vec![region as usize], Duration::from_secs(120));
            }
            say!("{name}: {size} bytes created and shared in {} µs", created.as_micros());
            unsafe {
                reply.set_value(name, shm);
                xpc_release(shm);
            }
            reply.set_u64(&format!("{name}_size"), size as u64);
            big.push((name, region as usize, size));
        }
    }

    /// Verifies what the plug-in wrote into the upper halves, and that r1
    /// is unchanged after the plug-in replaced its own mapping of it.
    fn check_regions(reply: &Dict) {
        for &(name, addr, size) in BIG.lock().unwrap().iter() {
            let base = addr as *const u64;
            if name != "r1" {
                let n = size / STRIDE;
                let mut good = 0;
                for i in n / 2..n {
                    let v = stride_pattern(false, size, i);
                    let (a, b) = unsafe {
                        (
                            ptr::read_volatile(base.add(i * STRIDE / 8)),
                            ptr::read_volatile(base.add(((i + 1) * STRIDE - 8) / 8)),
                        )
                    };
                    good += (a == v && b == v) as usize;
                }
                say!("check {name}: {good} of {} plug-in strides visible", n - n / 2);
                reply.set_u64(&format!("{name}_good"), good as u64);
                reply.set_u64(&format!("{name}_want"), (n - n / 2) as u64);
            } else {
                let pages = size / PAGE;
                let intact = (0..pages)
                    .filter(|&p| unsafe { ptr::read_volatile(base.add(p * PAGE / 8)) } == 0xD1D1_0000 | p as u64)
                    .count();
                say!("check {name}: {intact} of {pages} daemon pages intact");
                reply.set_u64(&format!("{name}_intact"), intact as u64);
                reply.set_u64(&format!("{name}_pages"), pages as u64);
            }
        }
    }
}
