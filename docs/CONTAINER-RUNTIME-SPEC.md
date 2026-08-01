# ri-agent Container Runtime Spec

> Rust tabanlı, rootless, image'siz sandbox runtime.
> Rootfs binary'de gömülü (`include_bytes!`). Network host ile shared.
> Sadece Rust — hiç OCI, hiç podman, hiç docker.

---

## 1. Felsefe

```
ri-agent binary (Rust)
├── include_bytes!("rootfs.tar.zst")  ← minimal rootfs gömülü
│   ├── /usr/bin/rustc
│   ├── /usr/bin/ld
│   ├── /usr/bin/sh
│   ├── /lib/libc.so (musl)
│   └── /lib/ld-musl.so
│
├── RiContainerRuntime (Rust syscall'lar)
│   ├── unshare(CLONE_NEWUSER)    ← root GEREKMEZ
│   ├── chroot + pivot_root       ← user namespace içinde
│   ├── mount bind /tools         ← custom tool dizini
│   └── exec                      ← tool çalıştır
│
└── Tool dispatch
    ├── bash  → /bin/sh -c (sandbox'ta)
    ├── exec  → direkt (sandbox'ta)
    ├── rustc → rustc (sandbox'ta)
    └── custom → /tools/{name} (sandbox'ta)
```

Hiç:
- ❌ OCI image formatı
- ❌ Podman / Docker / youki
- ❌ Root yetkisi
- ❌ Network izolasyonu (host ile shared → internet var)
- ❌ Disk'te ayrı rootfs (binary'de gömülü)

---

## 2. Gömülü Rootfs

### Binary'de Embed

```rust
// build.rs — compile time'da rootfs'i sıkıştırıp binary'e göm

/// Build script: ri-agent build alırken rootfs'i hazırla.
fn main() {
    // 1. rustc + ld + musl lib + sh + busybox'ı al
    // 2. minimal rootfs dizini oluştur
    // 3. zstd ile sıkıştır
    // 4. include_bytes! ile binary'e göm

    println!("cargo:rustc-env=RI_ROOTFS={}", rootfs_path);
}
```

```rust
// src/container/rootfs.rs

/// Binary'de gömülü rootfs (zstd compressed tar).
/// İlk çalıştırmada ~/.cache/ri/sandbox/ dizinine extract edilir.
pub static EMBEDDED_ROOTFS: &[u8] =
    include_bytes!(env!("RI_ROOTFS"));
```

### Rootfs İçeriği

```
rootfs/                          ← ~/.cache/ri/sandbox/ (extract)
├── usr/
│   ├── bin/
│   │   ├── rustc               ← ~2.5MB (strip edilmiş)
│   │   ├── ld                  ← ~700KB (GNU ld, musl target)
│   │   └── sh                  ← ~100KB (busybox ash)
│   └── lib/
│       ├── libc.so             ← musl libc
│       ├── ld-musl-x86_64.so   ← musl dynamic linker
│       └── libgcc_s.so         ← GCC runtime (rustc için)
│
├── root/.rustup/
│   └── toolchains/stable-.../
│       └── lib/rustlib/
│           └── x86_64-unknown-linux-musl/
│               ├── lib/*.rlib  ← Rust std (musl target)
│               └── self-contained/
│                   ├── libc.a  ← statik musl libc
│                   ├── crt1.o
│                   └── ...
│
├── tmp/                         ← çalışma dizini
└── etc/
    └── resolv.conf              ← DNS (internet için)
```

**Hedef boyut:** ~40-50MB (zstd sıkıştırılmış, runtime extract)

---

## 3. Rootless: User Namespace ile Root Gerekmez

### Neden CLONE_NEWUSER?

`unshare(CLONE_NEWUSER)` **root gerektirmez** — her Linux kullanıcısı kendi user namespace'i yaratabilir:

```bash
# Kullanıcı namespace'i içinde:
#   - process kendi namespace'inde CAP_FULL yetkisine sahiptir
#   - pivot_root, mount, chroot yapabilir
#   - Ama gerçek root'a ihtiyaç yok
#   - Kernel, user namespace'i izole eder
```

### User Namespace Detayları

```
Host (root):
  ┌─ ri-agent process (normal user) ────┐
  │ unshare(CLONE_NEWUSER)               │
  │   ↓                                  │
  │ User namespace (yeni UID/GID map)    │
  │   ↓                                  │
  │ UID 0 (fake root) içinde:            │
  │   ├── CAP_SYS_ADMIN (mount için)     │
  │   ├── CAP_NET_ADMIN (network için)   │
  │   └── pivot_root, chroot, exec       │
  └──────────────────────────────────────┘
```

### ID Mapping

```rust
fn setup_id_mapping(pid: libc::pid_t) -> Result<()> {
    // /proc/{pid}/uid_map ve gid_map'i ayarla
    // Kullanıcının UID'sini container içinde 0'a map et

    let uid = libc::getuid();
    let gid = libc::getgid();

    // /proc/{pid}/uid_map:
    //   "0 {uid} 1\n"    → container UID 0 = host UID {uid}
    // /proc/{pid}/gid_map:
    //   "0 {gid} 1\n"    → container GID 0 = host GID {gid}

    // Not: /proc/{pid}/setgroups dosyasını önce "deny" yaz
    // (user namespace güvenlik kuralı)
    
    write_file(format!("/proc/{pid}/uid_map"), format!("0 {uid} 1\n"))?;
    write_file(format!("/proc/{pid}/gid_map"), format!("0 {gid} 1\n"))?;

    Ok(())
}
```

---

## 4. RiContainerRuntime (Rootless)

### Core Struct

```rust
// ri-agent/src/container/mod.rs

/// Rootless, image'siz Rust sandbox runtime.
/// 
/// # Flow
/// 1. unshare(CLONE_NEWUSER) → user namespace (root gerekmez)
/// 2. clone → child (yeni namespace'de)
/// 3. child: setup_id_mapping → UID 0 mapping
/// 4. child: chroot(extracted_rootfs)
/// 5. child: mount bind /tools ve /tmp
/// 6. child: exec(tool_command)
/// 7. parent: waitpid, collect output
///
/// # Network
/// Network namespace izole edilmez → host ile shared → internet var.
pub struct RiContainerRuntime {
    /// Extract edilmiş rootfs dizini (~/.cache/ri/sandbox/).
    rootfs: PathBuf,

    /// Host'taki custom tool dizini (bind mount).
    tools_dir: PathBuf,

    /// Host'taki tmp dizini (bind mount).
    tmp_dir: PathBuf,
}

impl RiContainerRuntime {
    /// Rootfs'i ilk çalıştırmada extract et, runtime'ı hazırla.
    pub fn init() -> Result<Self> {
        let cache_dir = dirs::cache_dir()?.join("ri").join("sandbox");
        let rootfs = cache_dir.join("rootfs");

        if !rootfs.exists() {
            Self::extract_rootfs(&rootfs)?;
        }

        Ok(Self {
            rootfs,
            tools_dir: dirs::home_dir()?.join(".ri").join("tools"),
            tmp_dir: PathBuf::from("/tmp"),
        })
    }

    /// Gömülü rootfs'i diske extract et.
    fn extract_rootfs(target: &Path) -> Result<()> {
        let compressed = EMBEDDED_ROOTFS;  // include_bytes!
        let decoder = zstd::Decoder::new(std::io::Cursor::new(compressed))?;
        let mut archive = tar::Archive::new(decoder);
        archive.extract(target)?;
        Ok(())
    }

    /// Bir komutu sandbox içinde çalıştır.
    pub fn run(&self, cmd: &str, args: &[&str]) -> Result<CommandOutput> {
        // -- implementation below --
    }
}
```

### Run Method

```rust
pub fn run(&self, cmd: &str, args: &[&str]) -> Result<CommandOutput> {
    // ── 1. Pipe setup ──────────────────────────────────
    let (stdout_r, stdout_w) = os_pipe::pipe()?;
    let (stderr_r, stderr_w) = os_pipe::pipe()?;

    // ── 2. User namespace + clone ──────────────────────
    // ÖNCE unshare(CLONE_NEWUSER) → user namespace yarat
    // (root gerekmez, her kullanıcı yapabilir)
    unsafe {
        libc::unshare(libc::CLONE_NEWUSER);
    }

    // Şimdi user namespace içindeyiz.
    // clone ile child yarat (yeni namespace'ler)
    let child_pid = unsafe {
        libc::clone(
            libc::CLONE_NEWNS | libc::SIGCHLD,
            None,
        )
    };

    match child_pid {
        Err(e) => return Err(Error::Syscall("clone", e)),
        Ok(0) => {
            // ── CHILD PROCESS (user namespace içinde) ───
            self.child_work(&stdout_w, &stderr_w, cmd, args)?
        }
        Ok(pid) => {
            // ── PARENT PROCESS ───────────────────────────
            // ID mapping'i ayarla
            setup_id_mapping(pid)?;

            return self.parent_work(pid, &stdout_r, &stderr_r);
        }
    }
}
```

### Child Work (Rootless)

```rust
fn child_work(
    &self,
    stdout_w: &RawFd,
    stderr_w: &RawFd,
    cmd: &str,
    args: &[&str],
) -> Result<()> {
    unsafe {
        libc::dup2(stdout_w, 1);
        libc::dup2(stderr_w, 2);
    }

    // ── chroot: rootfs'e geç ──────────────────────────
    // NOT: pivot_root yerine chroot kullanıyoruz
    // Çünkü user namespace'de chroot daha basit ve yeterli
    unsafe {
        libc::chroot(self.rootfs.as_os_str().as_bytes())?;
    }
    std::env::set_current_dir("/")?;

    // ── Mount bind /tools ve /tmp ─────────────────────
    // User namespace içinde mount yapabiliriz
    // (fake root yetkisi ile)
    unsafe {
        libc::mount(
            self.tools_dir.as_os_str().as_bytes(),
            "/tools",
            libc::MS_BIND | libc::MS_RDONLY,
            0,
        );

        libc::mount(
            self.tmp_dir.as_os_str().as_bytes(),
            "/tmp",
            libc::MS_BIND,
            0,
        );

        // /proc'u mount et (rustc için gerekli)
        libc::mount(
            "proc\0".as_ptr() as *const _,
            "/proc\0".as_ptr() as *const _,
            "proc\0".as_ptr() as *const _,
            libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID,
            0,
        );
    }

    // ── exec ────────────────────────────────────────────
    let ccmd = CString::new(cmd)?;
    let cargs: Vec<CString> = args.iter()
        .map(|a| CString::new(*a))
        .collect::<Result<_, _>>()?;

    unsafe {
        libc::execvp(ccmd.as_ptr(), cargs.as_ptr());
        libc::_exit(1);
    }
}
```

### Parent Work

```rust
fn parent_work(
    &self,
    child_pid: libc::pid_t,
    stdout_r: &RawFd,
    stderr_r: &RawFd,
) -> Result<CommandOutput> {
    // ── Async stdout/stderr oku ────────────────────────
    // tokio::io::AsyncReadExt veya blocking okuyucu
    let stdout = read_pipe(stdout_r)?;
    let stderr = read_pipe(stderr_r)?;

    // ── Waitpid ────────────────────────────────────────
    let mut status: i32 = 0;
    unsafe { libc::waitpid(child_pid, &mut status, 0); }

    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        -(libc::WTERMSIG(status))
    } else {
        -1
    };

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        exit_code,
    })
}

fn read_pipe(fd: &RawFd) -> Result<Vec<u8>> {
    let mut buf = [0u8; 65536];
    let mut acc = Vec::new();
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr(), 65536) };
        if n <= 0 { break; }
        acc.extend_from_slice(&buf[..n as usize]);
    }
    Ok(acc)
}
```

---

## 5. Tool Dispatch

### Tool Proxy'leri

```rust
// ── BashTool ──────────────────────────────────────────
impl Tool for BashTool {
    fn run(&self, args, ctx) -> Pin<Box<dyn Future<Output=ToolResult>>> {
        let runtime = RiContainerRuntime::init()?;
        let command = args.get("command").and_then(|v| v.as_str())?;
        // Sandbox'ta /bin/sh -c çalıştır
        runtime.run("/bin/sh", &["-c", command])
    }
}

// ── ExecTool ──────────────────────────────────────────
impl Tool for ExecTool {
    fn run(&self, args, ctx) -> Pin<Box<dyn Future<Output=ToolResult>>> {
        let runtime = RiContainerRuntime::init()?;
        runtime.run(args.program, &args.args)
    }
}

// ── CustomTool ────────────────────────────────────────
impl Tool for CustomTool {
    fn run(&self, args, ctx) -> Pin<Box<dyn Future<Output=ToolResult>>> {
        let runtime = RiContainerRuntime::init()?;
        // Custom tool binary'si sandbox'taki /tools/{name}
        runtime.run("/tools/my-tool", &[&args_json])
    }
}

// ── RustcTool (Bootstrapping) ─────────────────────────
impl Tool for RustcTool {
    fn run(&self, args, ctx) -> Pin<Box<dyn Future<Output=ToolResult>>> {
        let runtime = RiContainerRuntime::init()?;
        // Sandbox'ta rustc var → musl binary üret
        // --target musl ile statik link
        // Çıktı /tools/my-tool → host ~/.ri/tools/'a yazılır
        runtime.run("rustc", &[
            "--target", "x86_64-unknown-linux-musl",
            "-C", "linker=/usr/bin/ld",
            "-o", "/tools/my-tool",
            "/tmp/source.rs",
        ])
    }
}
```

---

## 6. Bootstrapping: Tool Üretimi

```
Kullanıcı: "bana bir tool yaz, şunu yapsın"

Agent:
  1. Rust source code üret
  2. source.rs'yi /tmp/source.rs'e yaz (sandbox'ta)
  3. rustc tool'u çağır:
     RiContainerRuntime::run("rustc", [
       "--target", "x86_64-unknown-linux-musl",
       "-o", "/tools/my-tool",
       "/tmp/source.rs"
     ])
  4. Sandbox içinde:
     rustc → .o üret
     ld → musl statik binary
     /tools/my-tool → yazılır
     (bind mount ile host ~/.ri/tools/my-tool'a yansır)
  5. Host'ta load_custom_tools() → my-tool'u görür
  6. Agent artık my-tool'u kullanabilir
  7. Sonraki çağrılar: RiContainerRuntime::run("/tools/my-tool", ...)
```

### Hot-Reload

```rust
fn load_custom_tools() -> Vec<CustomTool> {
    let tools_dir = dirs::home_dir()?.join(".ri").join("tools");
    let mut tools = Vec::new();

    for entry in fs::read_dir(tools_dir)? {
        let path = entry?.path();
        if path.is_file() && is_executable(&path) {
            // Tool'u sandbox içinde --describe ile sorgula
            let runtime = RiContainerRuntime::init()?;
            let output = runtime.run(path.to_str().unwrap(), &["--describe"])?;
            // output.stdout → JSON parse → CustomTool oluştur
        }
    }
    Ok(tools)
}
```

---

## 7. Güvenlik

### Rootless Güvenlik Katmanları

```rust
// ── User namespace ─────────────────────────────────────
// CLONE_NEWUSER ile:
//   - Kernel, user namespace'i ana namespace'den izole eder
//   - Container içinde CAP_FULL var ama ana host'a dokunamaz
//   - pivot_root / chroot ile host fs'si görünmez
//   - Network host ile shared (internet erişimi)

// ── Mount namespace ────────────────────────────────────
// CLONE_NEWNS ile:
//   - chroot ile rootfs sandbox'a hapsolur
//   - /tools ve /tmp sadece bind mount noktaları
//   - Ana host'un /usr, /etc, /proc'sini görmez

// ── Seccomp filter ─────────────────────────────────────
// Seccomp BPF (rootless da çalışır):
//   - user namespace içinde seccomp ayarlanabilir
//   - Sadece rustc için gerekli syscall'lar
//   - Bloklu: bpf, reboot, acct, init_module, ...
```

```rust
// ── Seccomp Filter (Rootless) ──────────────────────────

fn setup_seccomp() -> Result<()> {
    use seccompiler::*;

    let filter = ScmpFilter::new(ScmpAction::Kill)?
        .add_rule(ScmpAction::Allow, Syscall::read)?
        .add_rule(ScmpAction::Allow, Syscall::write)?
        .add_rule(ScmpAction::Allow, Syscall::open)?     // rustc: dosya açar
        .add_rule(ScmpAction::Allow, Syscall::execve)?    // rustc: linker çağırır
        .add_rule(ScmpAction::Allow, Syscall::mmap)?      // rustc: bellek haritası
        .add_rule(ScmpAction::Allow, Syscall::brk)?       // rustc: heap
        .add_rule(ScmpAction::Allow, Syscall::stat)?      // rustc: dosya bilgisi
        .add_rule(ScmpAction::Allow, Syscall::clone)?     // rustc: thread
        .add_rule(ScmpAction::Allow, Syscall::futex)?     // rustc: sync
        .add_rule(ScmpAction::Allow, Syscall::connect)?   // internet: crates.io
        .add_rule(ScmpAction::Allow, Syscall::sendto)?    // internet: DNS
        .add_rule(ScmpAction::Allow, Syscall::recvfrom)?  // internet: HTTP
        .build()?;

    filter.apply()?;
    Ok(())
}
```

### Kaynak Sınırlamaları

```rust
// User namespace içinde RLIMIT ayarları:
fn setup_rlimits() -> Result<()> {
    let rlimits = [
        (libc::RLIMIT_NPROC,  64),     // max 64 process
        (libc::RLIMIT_NOFILE, 1024),   // max 1024 fd
        (libc::RLIMIT_AS,     512 * 1024 * 1024),  // max 512MB RAM
        (libc::RLIMIT_CPU,    30),     // max 30s CPU
    ];

    for (resource, limit) in rlimits {
        let rlim = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        unsafe { libc::setrlimit(resource, &rlim); }
    }
    Ok(())
}
```

---

## 8. Network: Host ile Shared

```rust
// Network namespace İZOLDE EDİLMEZ.
// CLONE_NEWNET kullanılmaz → host'un network'ü direkt görünür.
// 
// Bu sayede:
//   - rustc: crates.io'ya erişebilir (cargo dep çekmek için)
//   - curl/wget: internetten veri çekebilir
//   - DNS: /etc/resolv.conf (host'tan veya sandbox'tan)
//   - Custom tool'lar API call yapabilir
//
// NOT: Network izole olmadığı için tool'lar host'un
// network'üne tam erişimli. Bu bir güvenlik açığı değil
// çünkü zaten user namespace + mount namespace ile
// host fs'sine erişemiyorlar.
```

---

## 9. Dosya Yapısı

```
ri-agent/
├── src/
│   ├── container/
│   │   ├── mod.rs              # RiContainerRuntime
│   │   ├── rootfs.rs           # include_bytes! + extract
│   │   ├── namespace.rs        # CLONE_NEWUSER + ID mapping
│   │   ├── mount.rs            # chroot + bind mount
│   │   ├── seccomp.rs          # seccomp filter
│   │   ├── exec.rs             # execvp child
│   │   └── pipe.rs             # async pipe I/O
│   │
│   ├── agent/
│   │   └── tools/
│   │       ├── bash.rs         # sandbox proxy
│   │       ├── exec.rs         # sandbox proxy
│   │       ├── custom.rs       # sandbox proxy
│   │       └── rustc.rs        # bootstrapping tool
│   │
│   └── main.rs
│
├── build.rs                    # rootfs build script
│
├── rootfs/                     # kaynak: rootfs dizini (build)
│   ├── usr/
│   │   ├── bin/
│   │   │   ├── rustc
│   │   │   ├── ld
│   │   │   └── sh (busybox)
│   │   └── lib/
│   │       ├── libc.so (musl)
│   │       └── ld-musl-x86_64.so
│   └── root/.rustup/.../x86_64-unknown-linux-musl/
│       └── lib/*.rlib + self-contained/*
│
└── docs/
    └── CONTAINER-RUNTIME-SPEC.md
```

---

## 10. Build Süreci

```bash
# 1. Rootfs hazırla
./build-rootfs.sh
# Çıktı: rootfs.tar.zst (~40MB)

# 2. ri-agent build al
cargo build --release
# build.rs: rootfs.tar.zst'ı include_bytes! ile binary'e gömer

# 3. ri-agent çalıştır
ri-agent
# İlk çalıştırmada:
#   ~/.cache/ri/sandbox/ dizinine rootfs'i extract et
#   RiContainerRuntime init
#   Agent loop başlar
```

### build.rs

```rust
// build.rs
fn main() {
    // rootfs dizinini al, zstd ile sıkıştır
    let rootfs_dir = Path::new("rootfs");
    let tar_file = Path::new("rootfs.tar.zst");

    // tar cf - rootfs/ | zstd -o rootfs.tar.zst
    let tar = Command::new("tar")
        .args(["-cf", "-", "--hard-link", &rootfs_dir])
        .stdout(Stdio::piped())
        .spawn()?;

    let zstd = Command::new("zstd")
        .args(["-o", &tar_file])
        .stdin(tar.stdout)
        .output()?;

    // include_bytes! için env variable
    println!("cargo:rustc-env=RI_ROOTFS={}", tar_file.display());
    println!("cargo:rerun-if-changed=rootfs/");
}
```

---

## 11. Rootfs Build Script

```bash
#!/bin/bash
# build-rootfs.sh — Minimal rootfs hazırla

ROOTFS_DIR="rootfs"
TARGET="x86_64-unknown-linux-musl"
RUSTUP_HOME="$HOME/.rustup"
TOOLCHAIN="$RUSTUP_HOME/toolchains/stable-$TARGET"

# ── Binary'ler ──
mkdir -p $ROOTFS_DIR/usr/bin

# rustc: sadece musl target binary'si (strip edilmiş)
cp $TOOLCHAIN/bin/rustc $ROOTFS_DIR/usr/bin/
strip $ROOTFS_DIR/usr/bin/rustc

# ld: GNU linker (musl target için)
cp /usr/bin/ld $ROOTFS_DIR/usr/bin/

# sh: busybox ash (minimal shell)
cp /bin/busybox $ROOTFS_DIR/usr/bin/sh

# ── Kütüphaneler ──
mkdir -p $ROOTFS_DIR/usr/lib

# musl libc
cp /lib/libc.so $ROOTFS_DIR/usr/lib/
cp /lib/ld-musl-x86_64.so $ROOTFS_DIR/usr/lib/

# ── Rust std library (musl target) ──
mkdir -p $ROOTFS_DIR/root/.rustup/toolchains
cp -r $TOOLCHAIN/lib/rustlib/$TARGET \
  $ROOTFS_DIR/root/.rustup/toolchains/stable-$TARGET/lib/rustlib/$TARGET

# ── DNS ──
cp /etc/resolv.conf $ROOTFS_DIR/etc/

# ── Sıkıştır ──
tar -cf - --hard-link $ROOTFS_DIR | zstd -o rootfs.tar.zst
echo "rootfs.tar.zst: $(du -sh rootfs.tar.zst | cut -f1)"
```

---

## 12. Akış

### İlk Çalıştırma

```
ri-agent start
  ├── ~/.cache/ri/sandbox/ yok mu?
  │   ├── EMBEDDED_ROOTFS (zstd tar) extract
  │   ├── ~/.cache/ri/sandbox/rootfs/
  │   └── RiContainerRuntime init
  │
  ├── Tool dispatch hazır
  │   ├── bash → RiContainerRuntime::run("/bin/sh", ...)
  │   ├── exec → RiContainerRuntime::run(...)
  │   ├── rustc → RiContainerRuntime::run("rustc", ...)
  │   └── custom → RiContainerRuntime::run("/tools/{name}", ...)
  │
  └── Agent loop başlar
```

### Tool Çağrısı

```
Agent: "ls ~/.ri/tools/"
  → bash tool
  → RiContainerRuntime::run("/bin/sh", ["-c", "ls /tools"])
  → unshare(CLONE_NEWUSER)  ← root gerekmez
  → clone + chroot
  → sandbox'ta /bin/sh -c "ls /tools"
  → stdout/stderr pipe ile host'a döner
  → Agent cevabı görür
```

### Bootstrapping

```
Agent: "bana bir tool yaz, dosya içeriğini analiz etsin"
  → Rust source üret
  → rustc tool
  → RiContainerRuntime::run("rustc", [
      "--target", "x86_64-unknown-linux-musl",
      "-o", "/tools/analyzer",
      "/tmp/analyzer.rs"
    ])
  → sandbox'ta derlenir, /tools/analyzer yazılır
  → host ~/.ri/tools/analyzer (bind mount)
  → load_custom_tools() görür
  → Agent analyzer'ı kullanabilir
```

### Güncelleme

```
ri-agent update
  → build-rootfs.sh çalıştır
  → cargo build (yeni rootfs binary'e gömülür)
  → yeni binary çalıştırılır
  → eski sandbox temizlenir
  → yeni rootfs extract edilir
```

---

## 13. Performans

| Operasyon | Önce (Podman Runtime) | Şimdi (ri Runtime) |
|-----------|:--------------------:|:------------------:|
| Sandbox başlatma | ~200ms | **~2ms** (unshare + clone) |
| Bellek kullanımı | ~50MB | **~1MB** (sadece process) |
| Binary boyutu | ~80MB (podman) | **+40MB** (rootfs gömülü) |
| Root gerek | ❌ (rootless podman) | ❌ (CLONE_NEWUSER) |
| Network | ✅ (host) | ✅ (host shared) |
| Tool çağrısı latency | ~50ms | **~0.5ms** |
| Disk'te rootfs | ~120MB (OCI image) | **~40MB** (extract edilmiş) |

---

## 14. Sınırlamalar

| Sınırlama | Sebep | Çözüm |
|-----------|-------|-------|
| **Linux only** | CLONE_NEWUSER, chroot, mount syscall'ları | Linux dışında WSL2 veya emulation |
| **User namespace desteği** | Kernel'de CONFIG_USER_NS gerekli | Çoğu modern kernel'da var |
| **~/.cache/ri/sandbox/** | İlk extract ~2 saniye | Sadece bir kez |
| **Network izole değil** | Host ile shared | İstenen özellik (internet erişimi) |
| **rootfs boyutu ~40MB** | rustc + musl target + rlib'ler | Strip + zstd ile optimize |

---

## Özet

```
┌────────────────────────────────────────────────────────┐
│                    ri-agent binary                     │
│  ┌──────────────────────────────────────┐              │
│  │ include_bytes!("rootfs.tar.zst")     │              │
│  │ ~40MB zstd sıkıştırılmış rootfs      │              │
│  │ rustc + ld + musl lib + busybox sh   │              │
│  └──────────────────────────────────────┘              │
│                                                        │
│  ┌──────────────────────────────────────┐              │
│  │ RiContainerRuntime                   │              │
│  │ ├── unshare(CLONE_NEWUSER)           │              │
│  │ ├── chroot(rootfs)                   │              │
│  │ ├── mount bind /tools                │              │
│  │ └── exec(cmd)                        │              │
│  └──────────────────────────────────────┘              │
│                                                        │
│  ┌──────────────────────────────────────┐              │
│  │ Tool dispatch (bash/exec/rustc/custom)│             │
│  │ → RiContainerRuntime::run()           │              │
│  └──────────────────────────────────────┘              │
└────────────────────────────────────────────────────────┘
```

**Özetle:**
- ✅ Rootfs binary'de gömülü (OCI image gerekmez)
- ✅ Root gerekmez (CLONE_NEWUSER ile)
- ✅ Internet erişimi var (host network shared)
- ✅ Sadece Rust — podman/youki/docker yok
- ✅ Bootstrapping: rustc ile yeni tool'lar üret
- ✅ Custom tool'lar musl statik binary
- ✅ ~40MB rootfs, ~2ms sandbox başlatma
