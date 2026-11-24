//! Root signatures.
//!
//! Serializing goes to the host's `D3D12SerializeRootSignature` (or its
//! versioned form): the description is laid out as on PC, and the blob it
//! returns works as the console's, whose two methods sit in the same slots.
//! The console's own flags (bit 24 and up) are dropped first.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;

use crate::object::{
    self, E_INVALIDARG, E_NOTIMPL, HRESULT, Iid, Object, ROOT_SIGNATURE, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const ROOT_SIGNATURE_IIDS: [Iid; 3] = [
    guid(
        0xc54a6b66,
        0x72df,
        0x4ee8,
        [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const CONSOLE_FLAGS: u32 = 0xff00_0000;

/// Flags only a host with shader model 6.6 dynamic resources accepts:
/// `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` (`0x400`) and
/// `SAMPLER_HEAP_DIRECTLY_INDEXED` (`0x800`). An older runtime (the WARP that
/// ships without the Agility SDK, vkd3d-proton under Wine) refuses the whole
/// description with `E_INVALIDARG`, so a refusal is retried without them: the
/// title keeps a usable root signature, and only shaders that index the heaps
/// directly, which those hosts cannot run either way, lose the flag.
const HEAP_FLAGS: u32 = 0xc00;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Desc {
    parameter_count: u32,
    parameters: *const c_void,
    sampler_count: u32,
    samplers: *const c_void,
    flags: u32,
}

/// Every version's description starts with the fields of [`Desc`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionedDesc {
    version: u32,
    desc: Desc,
}

type Serialize =
    unsafe extern "system" fn(*const Desc, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
type SerializeVersioned =
    unsafe extern "system" fn(*const VersionedDesc, *mut *mut c_void, *mut *mut c_void) -> HRESULT;

/// An export of the host's `d3d12.dll` in System32, looked up once.
fn host_export(cache: &OnceLock<Option<usize>>, name: &CStr) -> Option<usize> {
    *cache.get_or_init(|| {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::LibraryLoader::{
                GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
            };
            let dll: Vec<u16> = "d3d12.dll".encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated name; System32 only.
            let module = unsafe {
                LoadLibraryExW(
                    dll.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return None;
            }
            // SAFETY: a loaded module and a NUL-terminated name.
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }.map(|f| f as usize)
        }
        #[cfg(not(windows))]
        None
    })
}

fn without_console_flags(desc: Desc) -> Desc {
    if desc.flags & CONSOLE_FLAGS != 0 {
        tracing::debug!(
            flags = format_args!("{:#x}", desc.flags),
            "console root signature flags dropped"
        );
    }
    Desc {
        flags: desc.flags & !CONSOLE_FLAGS,
        ..desc
    }
}

/// `d3d12_xs!D3D12SerializeRootSignature`.
pub extern "system" fn serialize(
    desc: *const Desc,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12SerializeRootSignature") else {
        tracing::warn!("no host d3d12.dll to serialize root signatures");
        return E_NOTIMPL;
    };
    // SAFETY: the export has this signature.
    let serialize = unsafe { std::mem::transmute::<usize, Serialize>(address) };
    serialize_with(serialize, without_console_flags(desc), version, blob, error)
}

/// Serializes through `serialize`, and if the host refuses the description
/// only because of the heap-directly-indexed flags, once more without them.
fn serialize_with(
    serialize: Serialize,
    desc: Desc,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the caller resolved the host export, which has this signature.
    let result = unsafe { serialize(&desc, version, blob, error) };
    if result >= 0 || desc.flags & HEAP_FLAGS == 0 {
        return result;
    }
    tracing::debug!(
        flags = format_args!("{:#x}", desc.flags),
        "host refused the heap-directly-indexed root signature flags; retrying without them"
    );
    retry_without_heap_flags(serialize, &desc, version, blob, error)
}

/// The second attempt of [`serialize_with`], with [`HEAP_FLAGS`] cleared.
fn retry_without_heap_flags(
    serialize: Serialize,
    desc: &Desc,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    let stripped = Desc {
        flags: desc.flags & !HEAP_FLAGS,
        ..*desc
    };
    // SAFETY: as in [`serialize_with`]: the same host export.
    unsafe { serialize(&stripped, version, blob, error) }
}

/// `d3d12_xs!D3D12SerializeVersionedRootSignature`.
pub extern "system" fn serialize_versioned(
    desc: *const VersionedDesc,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12SerializeVersionedRootSignature") else {
        tracing::warn!("no host d3d12.dll to serialize root signatures");
        return E_NOTIMPL;
    };
    let desc = VersionedDesc {
        desc: without_console_flags(desc.desc),
        ..desc
    };
    // SAFETY: the export has this signature.
    let serialize = unsafe { std::mem::transmute::<usize, SerializeVersioned>(address) };
    serialize_versioned_with(serialize, desc, blob, error)
}

/// [`serialize_with`] through the versioned entry point, whose description
/// starts with the same [`Desc`].
fn serialize_versioned_with(
    serialize: SerializeVersioned,
    desc: VersionedDesc,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the caller resolved the host export, which has this signature.
    let result = unsafe { serialize(&desc, blob, error) };
    if result >= 0 || desc.desc.flags & HEAP_FLAGS == 0 {
        return result;
    }
    tracing::debug!(
        flags = format_args!("{:#x}", desc.desc.flags),
        "host refused the heap-directly-indexed root signature flags; retrying without them"
    );
    let stripped = VersionedDesc {
        desc: Desc {
            flags: desc.desc.flags & !HEAP_FLAGS,
            ..desc.desc
        },
        ..desc
    };
    // SAFETY: as above: the same host export.
    unsafe { serialize(&stripped, blob, error) }
}

type Deserialize =
    unsafe extern "system" fn(*const u8, usize, *const Iid, *mut *mut c_void) -> HRESULT;

/// `d3d12_xs!D3D12CreateRootSignatureDeserializer`: our blobs are the
/// host's serialization, so the host's deserializer reads them, and its
/// object (IUnknown, then the description getter) is laid out as the
/// console's. The title's upscaler asks with the PC interface id.
pub extern "system" fn create_deserializer(
    blob: *const u8,
    size: usize,
    iid: *const Iid,
    out: *mut *mut c_void,
) -> HRESULT {
    if blob.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12CreateRootSignatureDeserializer") else {
        tracing::warn!("no host d3d12.dll to deserialize root signatures");
        return E_NOTIMPL;
    };
    // SAFETY: the export has this signature; it takes what the title passed.
    unsafe { std::mem::transmute::<usize, Deserialize>(address)(blob, size, iid, out) }
}

#[repr(C)]
struct RootSignature {
    object: Object,
    /// The serialized form, kept for translating pipelines later.
    blob: Vec<u8>,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    _nodes: u32,
    blob: *const u8,
    size: usize,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if blob.is_null() || size == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `size` bytes.
    let blob = unsafe { std::slice::from_raw_parts(blob, size) }.to_vec();
    tracing::debug!(size, "root signature");
    let root = Box::new(RootSignature {
        object: Object::new(&FUNCTIONS),
        blob,
    });
    object::hand_out(ROOT_SIGNATURE, root, iid, &ROOT_SIGNATURE_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ROOT_SIGNATURE_IIDS).then_some(this);
    object::answer(ROOT_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<RootSignature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ROOT_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    /// One 32-bit constant at b0, serialized by the host, then created.
    #[cfg(windows)]
    #[test]
    fn host_serialized_blobs_create_root_signatures() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_ROOT_CONSTANTS, D3D12_ROOT_PARAMETER, D3D12_ROOT_PARAMETER_0,
            D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, D3D12_SHADER_VISIBILITY_ALL,
        };
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 1,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let desc = Desc {
            parameter_count: 1,
            parameters: (&raw const parameter).cast(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 0x0100_0001,
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(serialize(&desc, 1, &mut blob, &mut error), S_OK);
        let blob = blob.cast_const().cast::<Object>();
        let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
        let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
        assert!(size > 0);
        let null = std::ptr::null();
        let mut root = null;
        assert_eq!(
            create(null, 0, data, size, &ROOT_SIGNATURE_IIDS[0], &mut root),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(root, 0x10)(root), 0);
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
    }

    /// The host's deserializer reads our blobs back (the upscaler's call).
    #[cfg(windows)]
    #[test]
    fn our_blobs_deserialize_on_the_host() {
        use windows::Win32::Graphics::Direct3D12::ID3D12RootSignatureDeserializer;
        use windows::core::Interface;
        let desc = VersionedDesc {
            version: 1,
            desc: Desc {
                parameter_count: 0,
                parameters: std::ptr::null(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 1,
            },
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
        let blob = blob.cast_const().cast::<Object>();
        let data = slot::<extern "system" fn(This) -> *const u8>(blob, 0x18)(blob);
        let size = slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob);
        let iid: Iid = ID3D12RootSignatureDeserializer::IID.to_u128().to_be_bytes();
        let iid = guid(
            u32::from_be_bytes(iid[0..4].try_into().unwrap()),
            u16::from_be_bytes(iid[4..6].try_into().unwrap()),
            u16::from_be_bytes(iid[6..8].try_into().unwrap()),
            iid[8..16].try_into().unwrap(),
        );
        let mut out = std::ptr::null_mut();
        assert_eq!(create_deserializer(data, size, &iid, &mut out), S_OK);
        // SAFETY: the host handed out this interface.
        let deserializer = unsafe { ID3D12RootSignatureDeserializer::from_raw(out) };
        // SAFETY: valid while the deserializer lives.
        let read = unsafe { &*deserializer.GetRootSignatureDesc() };
        assert_eq!((read.NumParameters, read.Flags.0), (0, 1));
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
    }

    /// Version 1.0 through the versioned entry point, with a console flag.
    #[cfg(windows)]
    #[test]
    fn versioned_descriptions_serialize_on_the_host() {
        let desc = VersionedDesc {
            version: 1,
            desc: Desc {
                parameter_count: 0,
                parameters: std::ptr::null(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 0x0200_0001,
            },
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
        let blob = blob.cast_const().cast::<Object>();
        assert!(slot::<extern "system" fn(This) -> usize>(blob, 0x20)(blob) > 0);
        slot::<extern "system" fn(This) -> u32>(blob, 0x10)(blob);
        assert_eq!(
            serialize_versioned(std::ptr::null(), &mut blob.cast_mut().cast(), &mut error),
            E_INVALIDARG
        );
    }

    /// The flags the fake host was asked to serialize, in order.
    static SEEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

    fn plain(flags: u32) -> Desc {
        Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags,
        }
    }

    /// A host without shader model 6.6 dynamic resources: it refuses a
    /// description that carries the heap-directly-indexed flags, as WARP
    /// without the Agility SDK and vkd3d-proton under Wine do.
    unsafe extern "system" fn refuses_heap_flags(
        desc: *const Desc,
        _: u32,
        _: *mut *mut c_void,
        _: *mut *mut c_void,
    ) -> HRESULT {
        // SAFETY: the tests pass a live description.
        let flags = unsafe { (*desc).flags };
        SEEN.lock().unwrap().push(flags);
        if flags & HEAP_FLAGS == 0 {
            S_OK
        } else {
            E_INVALIDARG
        }
    }

    /// [`refuses_heap_flags`] for the versioned entry point.
    unsafe extern "system" fn refuses_heap_flags_versioned(
        desc: *const VersionedDesc,
        _: *mut *mut c_void,
        _: *mut *mut c_void,
    ) -> HRESULT {
        // SAFETY: the tests pass a live description.
        let flags = unsafe { (*desc).desc.flags };
        SEEN.lock().unwrap().push(flags);
        if flags & HEAP_FLAGS == 0 {
            S_OK
        } else {
            E_INVALIDARG
        }
    }

    /// A host that refuses every description, for whatever reason.
    unsafe extern "system" fn refuses_everything(
        _: *const Desc,
        _: u32,
        _: *mut *mut c_void,
        _: *mut *mut c_void,
    ) -> HRESULT {
        E_INVALIDARG
    }

    /// A refusal of the heap-directly-indexed flags is retried without them,
    /// and nothing else is retried or changed.
    #[test]
    fn the_heap_flags_are_dropped_only_after_the_host_refuses_them() {
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        let seen = || SEEN.lock().unwrap().clone();

        SEEN.lock().unwrap().clear();
        assert_eq!(
            serialize_with(refuses_heap_flags, plain(0xc01), 1, &mut blob, &mut error),
            S_OK
        );
        assert_eq!(seen(), [0xc01, 0x1], "the retry keeps every other flag");

        SEEN.lock().unwrap().clear();
        assert_eq!(
            serialize_with(refuses_heap_flags, plain(0x41), 1, &mut blob, &mut error),
            S_OK
        );
        assert_eq!(seen(), [0x41], "no heap flags, one attempt");

        SEEN.lock().unwrap().clear();
        assert_eq!(
            serialize_versioned_with(
                refuses_heap_flags_versioned,
                VersionedDesc {
                    version: 1,
                    desc: plain(0xc03)
                },
                &mut blob,
                &mut error
            ),
            S_OK
        );
        assert_eq!(seen(), [0xc03, 0x3]);

        SEEN.lock().unwrap().clear();
        assert_eq!(
            serialize_with(refuses_everything, plain(0xc01), 1, &mut blob, &mut error),
            E_INVALIDARG,
            "a refusal that is not about the flags comes back unchanged"
        );
    }
}
