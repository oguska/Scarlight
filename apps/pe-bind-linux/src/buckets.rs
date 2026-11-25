//! The three tables, and the rule that orders them.
//!
//! * [`Bucket::Hle`] — victory-hle implements this import today, on the same
//!   MS x64 ABI the title calls it with, so a Linux host keeps it as-is.
//! * [`Bucket::Native`] — no Windows semantics are involved: a libc, pthread,
//!   mmap or futex call does the same job, and the shape of the object the
//!   title holds does not cross the boundary.
//! * [`Bucket::Windows`] — the *ABI itself* is the interface: SEH/unwind
//!   tables, TLS callbacks, MSVC C++ exceptions, RTTI, `std::` containers,
//!   MSVC's `FILE*`/printf/locale, or a kernel object model with no Linux
//!   equivalent (registry, COM, DStorage, XAudio2, Media Foundation).
//! * [`Bucket::Gap`] — not in any of the three: an import we have neither
//!   implemented nor found a Linux answer for. The list is the work.
//!
//! Everything here is name-based and deliberately conservative: an import
//! lands in `Native` only if the mapping is unambiguous.

use std::collections::HashSet;
use std::fs;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Hle = 0,
    Native = 1,
    Windows = 2,
    /// Imported from a DLL the title ships beside it: bound by looking up that
    /// DLL's own export directory, which is what the loader already does on
    /// Windows and needs no platform underneath.
    Title = 3,
    Gap = 4,
}

/// Per-DLL counts, indexed by `Bucket as usize`.
pub type Row = [u64; 5];

pub struct Classifier {
    /// `<dll-glob>\t<export name>`, generated from victory-hle by
    /// `gen-hle-exports.py`.
    hle: HashSet<(String, String)>,
    hle_globs: Vec<String>,
    /// DLLs shipped beside the image, lower-case name -> named exports.
    title: Vec<(String, HashSet<String>)>,
}

impl Classifier {
    pub fn load(path: &std::path::Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut hle = HashSet::new();
        let mut globs = Vec::new();
        for line in text.lines() {
            let Some((dll, name)) = line.split_once('\t') else { continue };
            let dll = dll.trim().to_ascii_lowercase();
            if dll.ends_with('*') && !globs.contains(&dll) {
                globs.push(dll.clone());
            }
            hle.insert((dll, name.trim().to_string()));
        }
        Ok(Self { hle, hle_globs: globs, title: Vec::new() })
    }

    /// Registers the DLLs shipped next to the image and their named exports.
    pub fn set_title_dlls(&mut self, dlls: Vec<(String, HashSet<String>)>) {
        self.title = dlls;
    }

    fn is_hle(&self, dll: &str, name: &str) -> bool {
        if self.hle.contains(&(dll.to_string(), name.to_string())) {
            return true;
        }
        self.hle_globs.iter().any(|glob| {
            let prefix = glob.trim_end_matches('*');
            dll.starts_with(prefix) && self.hle.contains(&(glob.clone(), name.to_string()))
        })
    }

    /// `Some(true)` when the DLL is one the title ships, and `bool` says
    /// whether the name is really in its export table.
    fn is_title(&self, dll: &str, name: &str) -> Option<bool> {
        self.title
            .iter()
            .find(|(file, _)| file == dll)
            .map(|(_, exports)| exports.contains(name))
    }

    pub fn classify(&self, dll: &str, name: Option<&str>) -> Bucket {
        let dll_l = dll.to_ascii_lowercase();
        // Ordinals are exported by number; the HLE table carries the Winsock
        // ones as `#N`, which is how the caller spells them.
        let Some(name) = name else {
            return Bucket::Gap;
        };
        if self.is_hle(&dll_l, name) {
            return Bucket::Hle;
        }
        if let Some(exported) = self.is_title(&dll_l, name) {
            return if exported { Bucket::Title } else { Bucket::Gap };
        }
        if NATIVE_CRT_TWINS.contains(&name) {
            return Bucket::Native;
        }
        if windows_semantics(&dll_l, name).is_some() {
            return Bucket::Windows;
        }
        if let Some(n) = name.strip_prefix('#').and_then(|s| s.parse::<u16>().ok()) {
            if dll_l == "ws2_32.dll" && WS2_32_ORDINALS.iter().any(|(o, _)| *o == n) {
                return Bucket::Native;
            }
            return Bucket::Gap;
        }
        if native_equivalents().iter().any(|(n, _)| *n == name) {
            return Bucket::Native;
        }
        if name.starts_with("??") || name.starts_with("_?_") {
            return Bucket::Windows;
        }
        Bucket::Gap
    }

    /// The Linux call that does the same job, for the report.
    pub fn native_equivalent(&self, name: &str) -> Option<&'static str> {
        if NATIVE_CRT_TWINS.contains(&name) {
            return Some("libc twin (same bytes in, same bytes out)");
        }
        if let Some(n) = name.strip_prefix('#').and_then(|s| s.parse::<u16>().ok()) {
            return WS2_32_ORDINALS
                .iter()
                .find(|(o, _)| *o == n)
                .map(|(_, equiv)| *equiv);
        }
        native_equivalents()
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, equiv)| *equiv)
    }
}

/// Imports whose interface *is* a Windows ABI, plus the kernel object models
/// with no Linux equivalent.
pub fn windows_semantics(dll: &str, name: &str) -> Option<&'static str> {
    // The MSVC C/C++ runtime, minus the byte-level libc twins below.
    if (dll.starts_with("msvcp") || dll.starts_with("vcruntime") || dll.starts_with("concrt"))
        && !NATIVE_CRT_TWINS.contains(&name)
    {
        return Some("MSVC C++ runtime: exceptions, RTTI, std layout");
    }
    if (dll.starts_with("api-ms-win-crt-") || dll == "ucrtbase.dll")
        && !NATIVE_CRT_TWINS.contains(&name)
    {
        return Some("MSVC CRT semantics (FILE*, printf, locale, errno)");
    }
    const SEH: &[&str] = &[
        "__C_specific_handler",
        "__C_specific_handler_noexcept",
        "__CxxFrameHandler4",
        "__CxxFrameHandler3",
        "_except_handler4_common",
        "_seh_filter_dll",
        "_seh_filter_exe",
        "SetUnhandledExceptionFilter",
        "UnhandledExceptionFilter",
        "AddVectoredExceptionHandler",
        "RemoveVectoredExceptionHandler",
        "AddVectoredContinueHandler",
        "RtlLookupFunctionEntry",
        "RtlVirtualUnwind",
        "RtlUnwindEx",
        "RtlCaptureContext",
        "RtlPcToFileHeader",
        "__RTDynamicCast",
        "__std_type_info_destroy_list",
        "__std_terminate",
        "_CxxThrowException",
        "terminate",
        "unexpected",
        "set_terminate",
        "set_unexpected",
        "_set_se_translator",
        "_getptd",
        "_initterm",
        "_initterm_e",
        "__security_init_cookie",
        "__security_check_cookie",
        "_guard_dispatch_icall",
        "_guard_check_icall",
    ];
    if SEH.contains(&name) {
        return Some("SEH/unwind/CFG ABI");
    }
    if name.starts_with("__Cxx") || name.contains("__FrameHandler") || name.contains("__TypeInfo") {
        return Some("MSVC C++ EH/RTTI internals");
    }
    // Kernel object models that are not a syscall away.
    const MODEL: &[&str] = &[
        "RegOpenKeyExW",
        "RegQueryValueExW",
        "RegCloseKey",
        "CoInitializeEx",
        "CoCreateInstance",
        "CoUninitialize",
        "SymInitialize",
        "SymFromAddr",
        "MiniDumpWriteDump",
    ];
    if MODEL.contains(&name) {
        return Some("Windows object model (registry/COM/dbghelp)");
    }
    if dll.starts_with("xaudio2") || dll.starts_with("mf") || dll.starts_with("winhttp")
        || dll.starts_with("dstorage") || dll.starts_with("mfplat")
    {
        return Some("Windows media/net object model");
    }
    None
}

/// Byte-level MSVC CRT functions whose behaviour and ABI *are* the Linux ones.
const NATIVE_CRT_TWINS: &[&str] = &[
    "memcpy",
    "memmove",
    "memset",
    "memcmp",
    "memchr",
    "strlen",
    "strcpy",
    "strncpy",
    "strcat",
    "strncat",
    "strcmp",
    "strncmp",
    "strchr",
    "strrchr",
    "strstr",
    "strspn",
    "strcspn",
    "strpbrk",
    "strtok",
    "malloc",
    "free",
    "calloc",
    "realloc",
    "_aligned_malloc",
    "_aligned_free",
    "abs",
    "labs",
    "qsort",
    "bsearch",
];

/// `(import name, the Linux call that does the same job)`.
///
/// A `None`-equivalent entry would be dishonest, so every row names a real
/// syscall or library function.
pub fn native_equivalents() -> &'static [(&'static str, &'static str)] {
    &[
        // memory
        ("VirtualAlloc", "mmap(PROT_*)"),
        ("VirtualAlloc2", "mmap; VK_EXT_map_memory_placed for GPU VAs"),
        ("VirtualFree", "munmap"),
        ("VirtualProtect", "mprotect"),
        ("VirtualQuery", "/proc/self/maps walk"),
        ("VirtualLock", "mlock"),
        ("VirtualUnlock", "munlock"),
        ("GetSystemInfo", "sysconf(_SC_PAGESIZE/_SC_NPROCESSORS_ONLN)"),
        ("GetNativeSystemInfo", "sysconf"),
        ("GlobalMemoryStatusEx", "/proc/meminfo"),
        ("GetWriteWatch", "/proc/self/pagemap PAGEMAP_SCAN"),
        ("ResetWriteWatch", "PAGEMAP_SCAN (clear soft-dirty)"),
        // threads
        ("CreateThread", "clone/pthread_create"),
        ("CreateRemoteThread", "clone into a thread of our own"),
        ("ExitThread", "pthread_exit"),
        ("GetExitCodeThread", "thread-local slot + pthread_join"),
        ("ResumeThread", "futex wake on a start gate"),
        ("SuspendThread", "tgkill(SIGSTOP) or a futex gate"),
        ("TerminateThread", "tgkill(SIGKILL) on a pthread"),
        ("SwitchToThread", "sched_yield"),
        ("Sleep", "nanosleep"),
        ("SleepEx", "nanosleep"),
        ("GetCurrentThreadId", "gettid"),
        ("GetCurrentProcessId", "getpid"),
        ("GetCurrentThread", "pthread_self"),
        ("GetCurrentProcess", "getpid"),
        ("SetThreadAffinityMask", "sched_setaffinity"),
        ("GetThreadTimes", "clock_gettime(CLOCK_THREAD_CPUTIME_ID)"),
        ("GetProcessTimes", "clock_gettime(CLOCK_PROCESS_CPUTIME_ID)"),
        ("SetThreadPriority", "sched_setscheduler/nice"),
        ("GetThreadPriority", "sched_getscheduler"),
        ("SetThreadDescription", "pthread_setname_np"),
        ("SetThreadStackGuarantee", "pthread_attr_setstack"),
        ("InitializeProcThreadAttributeList", "no-op"),
        // sync
        ("WaitForSingleObject", "futex(FUTEX_WAIT) or a condvar"),
        ("WaitForSingleObjectEx", "futex(FUTEX_WAIT)"),
        ("WaitForMultipleObjects", "epoll/eventfd set + futex"),
        ("WaitForMultipleObjectsEx", "epoll/eventfd set + futex"),
        ("CreateEventW", "futex word + atomic"),
        ("CreateEventA", "futex word + atomic"),
        ("CreateEventExW", "futex word + atomic"),
        ("SetEvent", "futex(FUTEX_WAKE)"),
        ("ResetEvent", "atomic store"),
        ("CreateMutexW", "pthread_mutex (recursive)"),
        ("ReleaseMutex", "pthread_mutex_unlock"),
        ("CreateSemaphoreW", "futex word + atomic"),
        ("ReleaseSemaphore", "futex(FUTEX_WAKE)"),
        ("InitializeCriticalSection", "pthread_mutex_init"),
        ("InitializeCriticalSectionAndSpinCount", "pthread_mutex_init"),
        ("InitializeCriticalSectionEx", "pthread_mutex_init"),
        ("EnterCriticalSection", "pthread_mutex_lock"),
        ("LeaveCriticalSection", "pthread_mutex_unlock"),
        ("DeleteCriticalSection", "pthread_mutex_destroy"),
        ("TryEnterCriticalSection", "pthread_mutex_trylock"),
        ("AcquireSRWLockExclusive", "pthread_rwlock_wrlock"),
        ("ReleaseSRWLockExclusive", "pthread_rwlock_unlock"),
        ("AcquireSRWLockShared", "pthread_rwlock_rdlock"),
        ("ReleaseSRWLockShared", "pthread_rwlock_unlock"),
        ("InitializeSRWLock", "pthread_rwlock_init"),
        ("InitOnceExecuteOnce", "pthread_once"),
        ("InitOnceBeginInitialize", "pthread_once"),
        ("InitOnceComplete", "pthread_once"),
        ("WakeAllConditionVariable", "pthread_cond_broadcast"),
        ("WakeConditionVariable", "pthread_cond_signal"),
        ("SleepConditionVariableSRW", "pthread_cond_wait"),
        ("CreateThreadpool", "pthread pool of our own"),
        ("CloseThreadpool", "pthread pool teardown"),
        ("SubmitThreadpoolWork", "pthread pool submit"),
        ("QueryPerformanceCounter", "clock_gettime(CLOCK_MONOTONIC)"),
        ("QueryPerformanceFrequency", "clock_gettime (1e9)"),
        ("GetSystemTimeAsFileTime", "clock_gettime(CLOCK_REALTIME) + epoch"),
        ("GetSystemTimePreciseAsFileTime", "clock_gettime(CLOCK_REALTIME)"),
        ("GetTickCount", "clock_gettime(CLOCK_MONOTONIC)"),
        ("GetTickCount64", "clock_gettime(CLOCK_MONOTONIC)"),
        ("timeGetTime", "clock_gettime(CLOCK_MONOTONIC)"),
        ("GetLocalTime", "localtime_r"),
        ("GetSystemTime", "gmtime_r"),
        ("SystemTimeToFileTime", "date arithmetic"),
        ("FileTimeToSystemTime", "date arithmetic"),
        ("GetTimeZoneInformation", "tzset/localtime_r"),
        // files
        ("CreateFileW", "open"),
        ("CreateFileA", "open"),
        ("ReadFile", "pread"),
        ("WriteFile", "pwrite"),
        ("ReadFileEx", "pread + aio"),
        ("WriteFileEx", "pwrite + aio"),
        ("FlushFileBuffers", "fsync"),
        ("SetFilePointer", "lseek"),
        ("SetFilePointerEx", "lseek"),
        ("GetFileSize", "fstat"),
        ("GetFileSizeEx", "fstat"),
        ("GetFileAttributesW", "stat"),
        ("GetFileAttributesExW", "stat"),
        ("SetFileAttributesW", "chmod/open flags"),
        ("DeleteFileW", "unlink"),
        ("MoveFileW", "rename"),
        ("MoveFileExW", "rename"),
        ("CopyFileW", "copy_file_range"),
        ("CreateDirectoryW", "mkdir"),
        ("RemoveDirectoryW", "rmdir"),
        ("FindFirstFileW", "opendir/readdir"),
        ("FindNextFileW", "readdir"),
        ("FindClose", "closedir"),
        ("GetFullPathNameW", "realpath"),
        ("GetCurrentDirectoryW", "getcwd"),
        ("SetCurrentDirectoryW", "chdir"),
        ("GetTempPathW", "getenv(TMPDIR)"),
        ("SetEndOfFile", "ftruncate"),
        ("LockFileEx", "fcntl(F_OFD_SETLK)"),
        ("CreateFileMappingW", "memfd_create/mmap"),
        ("MapViewOfFile", "mmap"),
        ("UnmapViewOfFile", "munmap"),
        ("FlushViewOfFile", "msync"),
        ("DeviceIoControl", "ioctl"),
        // process / environment
        ("GetModuleFileNameW", "readlink(/proc/self/exe)"),
        ("GetModuleHandleW", "dlopen handle or the title image"),
        ("GetModuleHandleA", "dlopen handle or the title image"),
        ("LoadLibraryW", "dlopen"),
        ("LoadLibraryA", "dlopen"),
        ("GetProcAddress", "dlsym"),
        ("FreeLibrary", "dlclose"),
        ("GetEnvironmentVariableW", "getenv"),
        ("SetEnvironmentVariableW", "setenv"),
        ("GetCommandLineW", "/proc/self/cmdline"),
        ("ExitProcess", "exit_group"),
        ("TerminateProcess", "kill"),
        ("GetExitCodeProcess", "waitpid status"),
        ("IsProcessorFeaturePresent", "cpuid"),
        ("IsDebuggerPresent", "always 0"),
        ("OutputDebugStringW", "write(2, stderr)"),
        ("OutputDebugStringA", "write(2, stderr)"),
        ("GetLastError", "thread-local errno twin"),
        ("SetLastError", "thread-local errno twin"),
        ("GetStdHandle", "fd 0/1/2 mapping"),
        ("CloseHandle", "close/free by handle kind"),
        ("DuplicateHandle", "dup"),
        ("SetHandleInformation", "fcntl(F_SETFL)"),
        ("HeapCreate", "malloc arenas"),
        ("HeapAlloc", "malloc"),
        ("HeapFree", "free"),
        ("HeapReAlloc", "realloc"),
        ("HeapSize", "malloc_usable_size"),
        ("HeapSetInformation", "no-op"),
        ("HeapValidate", "malloc_usable_size walk"),
        ("ProcessIdToSessionId", "always session 0"),
        ("GetSystemTimes", "clock_gettime + /proc/stat"),
        ("GetProcessMemoryInfo", "/proc/self/statm"),
        ("QueryThreadCycleTime", "rdtsc"),
        ("GetQueuedCompletionStatus", "epoll_wait"),
        ("CreateIoCompletionPort", "epoll_create1"),
        // console / stdio
        ("GetConsoleOutputCP", "always 65001"),
        ("GetConsoleMode", "no-op (fd is not a tty)"),
        ("SetConsoleMode", "no-op"),
        ("IsProcessorFeaturePresentEx", "cpuid"),
        // text / encoding / locale
        ("MultiByteToWideChar", "our own UTF-8<->UTF-16 (already in victory-hle)"),
        ("WideCharToMultiByte", "our own UTF-8<->UTF-16"),
        ("lstrcmpW", "wcscmp"),
        ("lstrcmpiW", "wcscasecmp"),
        ("lstrcpyW", "wcscpy"),
        ("lstrlenW", "wcslen"),
        ("GetDateFormatEx", "strftime + Windows format-string translation"),
        ("GetTimeFormatEx", "strftime + Windows format-string translation"),
        // misc kernel surface
        ("FlushInstructionCache", "no-op on x86-64 (coherent I-cache)"),
        ("GetVersionExW", "constant (kernel version is ours)"),
        ("GetVersion", "constant"),
        ("GetFileType", "fstat"),
        ("GetOverlappedResult", "our own AIO state"),
        ("GetOverlappedResultEx", "our own AIO state"),
        ("CancelIoEx", "io_uring/aio_cancel"),
        ("SetFileCompletionNotificationModes", "no-op"),
        ("GetFinalPathNameByHandleW", "readlink(/proc/self/fd/N)"),
        ("GetVolumeInformationW", "statfs"),
        ("GetDiskFreeSpaceExW", "statvfs"),
        ("QueryDosDeviceW", "no-op (no DOS namespace)"),
        ("SetErrorMode", "no-op (no WER dialogs)"),
        ("GetCurrentThreadStackLimits", "/proc/self/maps"),
        ("GetThreadGroupAffinity", "sched_getaffinity"),
        ("SetThreadGroupAffinity", "sched_setaffinity"),
        ("GetLogicalProcessorInformation", "sysfs topology"),
        ("GetLogicalProcessorInformationEx", "sysfs topology"),
        ("WerRegisterFile", "no-op (no Windows Error Reporting)"),
        ("RegisterAppConstrainedChangeNotification", "no-op (console-only notification)"),
        ("UnregisterAppConstrainedChangeNotification", "no-op"),
        ("InitializeSListHead", "atomic store"),
        ("LocalAlloc", "malloc"),
        ("LocalFree", "free"),
        ("CoCreateGuid", "getrandom"),
        ("CoTaskMemAlloc", "malloc"),
        ("CoTaskMemFree", "free"),
        ("StringFromGUID2", "uuid formatting of our own"),
        ("PropVariantClear", "our own COM shim"),
        ("BCryptGenRandom", "getrandom"),
        ("BCryptOpenAlgorithmProvider", "our own shim over the kernel RNG"),
        ("BCryptCloseAlgorithmProvider", "our own shim"),
        ("CreateNamedPipeW", "mkfifo"),
        ("SetNamedPipeHandleState", "fcntl"),
        ("WaitNamedPipeW", "open with a retry loop"),
        // sockets: the Winsock surface is BSD sockets plus a WSA error code
        ("WSAStartup", "no-op"),
        ("WSACleanup", "no-op"),
        ("WSAGetLastError", "thread-local errno twin"),
        ("WSASetLastError", "thread-local errno twin"),
        ("socket", "socket"),
        ("connect", "connect"),
        ("bind", "bind"),
        ("listen", "listen"),
        ("accept", "accept"),
        ("send", "send"),
        ("recv", "recv"),
        ("sendto", "sendto"),
        ("recvfrom", "recvfrom"),
        ("closesocket", "close"),
        ("select", "poll/epoll"),
        ("ioctlsocket", "ioctl"),
        ("setsockopt", "setsockopt"),
        ("getsockopt", "getsockopt"),
        ("getpeername", "getpeername"),
        ("getsockname", "getsockname"),
        ("shutdown", "shutdown"),
        ("htonl", "htonl"),
        ("htons", "htons"),
        ("ntohl", "ntohl"),
        ("ntohs", "ntohs"),
        ("inet_addr", "inet_pton"),
        ("inet_ntoa", "inet_ntop"),
        ("GetAddrInfoW", "getaddrinfo"),
        ("freeaddrinfo", "freeaddrinfo"),
        ("FreeAddrInfoW", "freeaddrinfo"),
        ("getaddrinfo", "getaddrinfo"),
        ("WSASend", "sendmsg"),
        ("WSARecv", "recvmsg"),
        ("WSAIoctl", "ioctl"),
        ("WSAEventSelect", "epoll + an eventfd"),
        ("WSAEnumNetworkEvents", "epoll_wait"),
        ("WSACreateEvent", "eventfd"),
        ("WSACloseEvent", "close"),
        ("WSAWaitForMultipleEvents", "epoll_wait"),
        // window / input: X11 or Wayland, through our own layer
        ("RegisterClassExA", "X11/Wayland window layer"),
        ("RegisterClassExW", "X11/Wayland window layer"),
        ("CreateWindowExA", "X11/Wayland window layer"),
        ("CreateWindowExW", "X11/Wayland window layer"),
        ("DestroyWindow", "X11/Wayland window layer"),
        ("DefWindowProcA", "X11/Wayland window layer"),
        ("DefWindowProcW", "X11/Wayland window layer"),
        ("ShowWindow", "X11/Wayland window layer"),
        ("PeekMessageA", "X11/Wayland event queue"),
        ("PeekMessageW", "X11/Wayland event queue"),
        ("GetMessageW", "X11/Wayland event queue"),
        ("DispatchMessageA", "X11/Wayland event queue"),
        ("DispatchMessageW", "X11/Wayland event queue"),
        ("TranslateMessage", "X11/Wayland key translation"),
        ("PostQuitMessage", "X11/Wayland event queue"),
        ("GetAsyncKeyState", "X11/Wayland keyboard state"),
        ("GetKeyState", "X11/Wayland keyboard state"),
        ("GetCursorPos", "X11/Wayland pointer"),
        ("SetCursorPos", "X11/Wayland pointer"),
        ("ClipCursor", "X11/Wayland pointer grab"),
        ("SetCapture", "X11/Wayland pointer grab"),
        ("ShowCursor", "X11/Wayland cursor"),
        ("SetWindowLongPtrW", "X11/Wayland window layer"),
        ("GetClientRect", "X11/Wayland window layer"),
        ("AdjustWindowRect", "our own arithmetic"),
        ("SetWindowPos", "X11/Wayland window layer"),
        ("SetWindowTextW", "X11/Wayland title"),
        ("UpdateWindow", "no-op (nothing is client-side buffered)"),
        ("GetSystemMetrics", "our own table"),
        ("MonitorFromWindow", "X11 RandR / wl_output"),
        ("GetMonitorInfoW", "X11 RandR / wl_output"),
        ("ChangeDisplaySettingsExW", "X11 RandR / wl_output"),
        ("LoadCursorW", "X11/Wayland cursor"),
        ("LoadIconW", "our own asset"),
        // remainder of the named gap list (all small shims)
        ("SetProcessPriorityBoost", "sched_setscheduler/nice"),
        ("GetProcessId", "pid of a handle we opened"),
        ("GetStartupInfoW", "constant empty STARTUPINFO"),
        ("OpenThread", "tgkill on a tid"),
        ("FileTimeToLocalFileTime", "localtime_r offset arithmetic"),
        ("LocalFileTimeToFileTime", "localtime_r offset arithmetic"),
        ("GetFileTime", "fstat"),
        ("SetFileTime", "futimens"),
        ("GetUserDefaultGeoName", "constant (no region store)"),
        ("GetCommandLineA", "/proc/self/cmdline"),
        ("VirtualQueryEx", "/proc/<pid>/maps"),
        ("FormatMessageA", "our own message table"),
        ("GetUserDefaultLocaleName", "setlocale/getenv(LANG)"),
        ("LocaleNameToLCID", "our own table"),
        ("CreateMutexA", "pthread_mutex (recursive)"),
        ("CreateSemaphoreExW", "futex word + atomic"),
    ]
}

/// Winsock 1.1 exports the title imports by ordinal (the numbers are fixed in
/// every `ws2_32.spec`, including Wine's): `(ordinal, BSD sockets call)`.
pub const WS2_32_ORDINALS: &[(u16, &str)] = &[
    (1, "accept"),
    (3, "close"),
    (5, "getpeername"),
    (6, "getsockname"),
    (7, "getsockopt"),
    (8, "htonl"),
    (9, "htons"),
    (10, "ioctl"),
    (11, "inet_pton"),
    (12, "inet_ntop"),
    (13, "listen"),
    (14, "ntohl"),
    (15, "ntohs"),
    (16, "recv"),
    (17, "recvfrom"),
    (18, "poll/epoll"),
    (19, "send"),
    (21, "setsockopt"),
    (22, "shutdown"),
    (23, "socket"),
    (57, "gethostname"),
    (111, "thread-local errno twin"),
    (115, "no-op (no WSA init needed)"),
    (151, "fd_set test"),
];
