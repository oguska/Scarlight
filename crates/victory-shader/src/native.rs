//! Native GFX10 programs that the console compiler stores in a `DXIL` part.
//!
//! An `XDAT` index after the program header lists blobs stored around the
//! bitcode; one or more of them hold native programs, zlib-packed (a vertex
//! shader can carry two: compiled for use with and without a geometry
//! shader). Layout as observed in shipped titles (index offsets are relative
//! to the part data):
//!
//! ```text
//! +0x18 "XDAT"  u32 entry count, u32 (0x24 seen), then per entry:
//!               u32 kind, u32 offset, u32 size
//! kind 0x0d     "XBCH" u32 version (3), u64 packed size, u64 unpacked size,
//!               zlib stream
//! unpacked      "FSSM" u32 version (0x101), u64 payload size, payload
//! payload +0x00 u64 graphics slot count (6)
//!         +0x08 u64 record offsets: LS HS ES GS VS PS (0x08..0x30),
//!               mesh 0x48, compute 0x60; unused ones are 0xDEADBEEF_DEADBEEF
//! record  +0x00 u64 code offset, +0x08 u64 code size (payload-relative),
//!         +0x14 u32 flags: bit 5 wave32 (else wave64)
//!         +0x28 u64 root layout table, +0x30 u64 used-resource table
//!               (payload-relative), +0x38 u32 and +0x3c u32 their counts
//!         +0x50 u32 bits 0-9 VGPRs used, bits 20-27 SGPRs used
//! layout  20 B: u32 root type, u32 table offset (dwords), u32 user SGPRs,
//!               u32 base register, u8 first user-data dword, u8 root slot,
//!               u16 0
//! used    20 B: u8 resource type, u8 root type, u16 0, u32 table offset
//!               (dwords), u32 lower bound, u32 stride, u8 descriptor
//!               dwords, u8 first user-data dword, u8 root slot, u8 0
//! ```
//!
//! Stage data follows the record; offsets from its end (E = record + 0x54):
//!
//! ```text
//! PS  E+0x24  32 inputs of 8 B: u8 usage, u8 signature row, u8 channel
//!             mask, u8 attribute (v_interp `attr`), u8 default, u8 second
//!             row (of a packed pair), u8 parameter slot, u8 flags (bit 0
//!             flat, 1 linear, 3 fp16, 4 fp16 packed); E+0x124 u32 count
//!     E+0x134 u32 SPI_PS_INPUT_ADDR, E+0x138 u32 DB_SHADER_CONTROL
//!     E+0x148 u32 export patch size (dwords), E+0x14c 8 patches, one per
//!             MRT: u32 code offset (dwords, -1 none), then the export code
//!             for each color format in 10 slots of 10 dwords (ZERO, 32_R,
//!             32_GR, 32_AR, FP16, UNORM16, SNORM16, UINT16, SINT16, 32_ABGR)
//! VS  E+0x24  32 inputs of 5 B: u8 usage, u8 row, u8 channel mask, u8 VGPR
//!             of channel 0 (channel k in VGPR + k), u8 highest channel;
//!             E+0xc4 u32 count
//!     E+0xc8  32 outputs of 6 B: u8 usage, u8 row, u8 parameter (`exp
//!             param<n>`), u8 row again, u8 slot, u8 flags (bit 0 fp16,
//!             bit 1 two fp16 values per dword); E+0x188 u32 count
//! CS  E+0x24  u32 COMPUTE_PGM_RSRC2, E+0x28 u32 x3 thread-group size
//! MS  E+0xc8  outputs as VS, E+0x188 count; the param byte's bit 7 marks a
//!             per-primitive output (`PSG1` row); the 3 bytes more are the
//!             second row of a packed pair, a slot, flags (bit 0 fp16, bit 1
//!             two fp16 elements packed)
//!     E+0x18c u32 SPI_VS_OUT_CONFIG, SPI_SHADER_POS_FORMAT,
//!             PA_CL_VS_OUT_CNTL, VGT_STRMOUT_CONFIG, SPI_SHADER_PGM_RSRC2_GS,
//!             VGT_GS_OUT_PRIM_TYPE, VGT_GS_INSTANCE_CNT, SPI_SHADER_IDX_FORMAT
//!     E+0x448 u16 thread-group size in lanes, u16 maximum vertices,
//!             u16 maximum primitives, u8 user SGPRs, u8 output topology
//!             (2 triangles); code follows at E+0x45c
//! record +0x4c u32 LDS bytes (`lds_byte_size`; mesh programs read it here)
//! ```
//!
//! Rows are registers of the container's signature parts (`ISG1`, `OSG1`).
//! Merged stages keep their stage data in another layout (not read).
//!
//! Other kinds seen: 0x07 program header + bitcode (or a stub when stripped),
//! 0x0a `xbop` (compiler options), 0x0e a table not decoded yet, 0x10 and
//! 0x12 16-byte blobs (0x10 is the shader hash).
//! Unknown versions are rejected so a format change shows up as an error.

use crate::ShaderError::{self, Malformed};
use crate::dxbc::{u32_at, u64_at};

const XDAT_OFFSET: usize = 0x18;
const MAX_XDAT_ENTRIES: usize = 64;
const KIND_NATIVE: u32 = 0x0d;
const XBCH_VERSION: u32 = 3;
const XBCH_HEADER: usize = 0x18;
const FSSM_VERSION: u32 = 0x101;
const FSSM_HEADER: usize = 0x10;
const GRAPHICS_SLOT_COUNT: u64 = 6;
/// Where each stage's record offset sits in the FSSM payload.
const SLOTS: [(HwStage, usize); 8] = [
    (HwStage::Ls, 0x08),
    (HwStage::Hs, 0x10),
    (HwStage::Es, 0x18),
    (HwStage::Gs, 0x20),
    (HwStage::Vs, 0x28),
    (HwStage::Ps, 0x30),
    (HwStage::Ms, 0x48),
    (HwStage::Cs, 0x60),
];
const ABSENT: u64 = 0xDEAD_BEEF_DEAD_BEEF;
const RECORD_SIZE: u64 = 0x54;
const RECORD_FLAGS: usize = 0x14;
const FLAG_WAVE32: u32 = 1 << 5;
const RECORD_REGS: usize = 0x50;
const RECORD_LAYOUT: usize = 0x28;
const RECORD_USED: usize = 0x30;
const RECORD_COUNTS: usize = 0x38;
const ENTRY_SIZE: u64 = 20;
/// `s0`-`s105`: the SGPRs a program can address (106 on is VCC).
const SGPRS: u32 = 106;
const MAX_SEMANTICS: u32 = 32;
const PS_INPUTS: usize = 0x24;
const PS_INPUT_COUNT: usize = 0x124;
const PS_INPUT_ADDR: usize = 0x134;
const PS_DB_SHADER_CONTROL: usize = 0x138;
const PS_PATCH_SIZE: usize = 0x148;
const PS_PATCHES: usize = 0x14c;
const PATCH_SLOT_DWORDS: usize = 10;
/// The variant that exports every channel as a raw 32-bit value.
const PATCH_FULL: usize = 9;
const PATCH_BYTES: usize = 4 + 10 * PATCH_SLOT_DWORDS * 4;
const MRTS: usize = 8;
const VS_INPUTS: usize = 0x24;
const VS_INPUT_COUNT: usize = 0xc4;
const VS_OUTPUTS: usize = 0xc8;
const VS_OUTPUT_COUNT: usize = 0x188;
const CS_RSRC2: usize = 0x24;
const CS_THREADS: usize = 0x28;
/// D3D12's `numthreads` limits: x and y up to 1024, z up to 64, and at
/// most 1024 threads in the group.
const MAX_GROUP: [u32; 3] = [1024, 1024, 64];
const MAX_GROUP_THREADS: u64 = 1024;
const RECORD_LDS: usize = 0x4c;
const MS_CLIP_CNTL: usize = 0x194;
const MS_INFO: usize = 0x448;
/// The LDS a GFX10 thread group can address.
const MAX_LDS_BYTES: u32 = 64 << 10;
/// D3D12's mesh limits: at most 128 threads in the group, and at most 256
/// vertices and 256 primitives out.
const MAX_MESH_THREADS: u32 = 128;
const MAX_MESH_OUTPUTS: u32 = 256;
/// Unpacked blobs are a few KiB; refuse to allocate for absurd sizes.
const MAX_UNPACKED: u64 = 16 << 20;

/// Hardware shader stage a native program runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HwStage {
    Ls,
    Hs,
    Es,
    Gs,
    Vs,
    Ps,
    /// Mesh shader.
    Ms,
    Cs,
}

impl HwStage {
    /// The SGPR holding user-data dword `index`: merged stages (and mesh
    /// shaders, which run as one) have 8 system SGPRs first.
    pub fn user_sgpr(self, index: u8) -> u8 {
        match self {
            HwStage::Ls | HwStage::Hs | HwStage::Es | HwStage::Gs | HwStage::Ms => index + 8,
            HwStage::Vs | HwStage::Ps | HwStage::Cs => index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgram {
    pub stage: HwStage,
    /// Runs as 64 lanes per wave (else 32); sizes lane masks.
    pub wave64: bool,
    /// VGPRs the program uses (v0 up to this), the bound of indexed access.
    pub vgprs: u16,
    /// Where the root signature's parameters arrive (user SGPRs).
    pub layout: Vec<RootParam>,
    /// The descriptors the program reads, by root parameter.
    pub used: Vec<UsedResource>,
    pub io: StageIo,
    /// GFX10 machine code, as stored.
    pub code: Vec<u8>,
}

impl NativeProgram {
    /// The code with each render target's export patched for its full
    /// 32-bit format, so exports carry the shader's values unconverted (as
    /// the console does at pipeline creation for that format).
    pub fn patched_code(&self) -> Vec<u8> {
        let mut code = self.code.clone();
        if let StageIo::Ps { patches, .. } = &self.io {
            for p in patches {
                let at = p.offset as usize * 4;
                for (i, word) in p.code.iter().enumerate() {
                    code[at + 4 * i..at + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
        }
        code
    }
}

/// What a semantic is, as the stage data names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Usage {
    Generic,
    ClipDistance,
    Other(u8),
}

impl Usage {
    fn from_raw(raw: u8) -> Self {
        match raw {
            6 => Usage::Generic,
            7 => Usage::ClipDistance,
            n => Usage::Other(n),
        }
    }
}

/// A pixel shader input: what `v_interp` of attribute `attr` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsInput {
    pub usage: Usage,
    /// Register in the input signature.
    pub row: u8,
    pub mask: u8,
    pub attr: u8,
    pub flat: bool,
    pub fp16: bool,
    /// Two 16-bit values per dword ([`VsOutput::pack2`]); `row2` is the
    /// second element's register.
    pub pack2: bool,
    pub row2: u8,
}

/// A vertex attribute the fetch shader loads: channel k into VGPR `vgpr + k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsInput {
    pub usage: Usage,
    pub row: u8,
    pub mask: u8,
    pub vgpr: u8,
}

/// What `exp param<param>` of a vertex shader writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsOutput {
    pub usage: Usage,
    /// Register in the output signature.
    pub row: u8,
    pub param: u8,
    /// 16-bit values, one per dword (low half) unless `pack2`: then two per
    /// dword, channel c of this row in dword c / 2 (odd channels in the
    /// high half), then a second 16-bit row from the next whole dword (the
    /// next 16-bit row of the signature no entry names).
    pub fp16: bool,
    pub pack2: bool,
}

/// The export code of one render target for its full 32-bit format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPatch {
    pub mrt: u8,
    /// Where it goes in the code, in dwords.
    pub offset: u32,
    pub code: Vec<u32>,
}

/// Inputs and outputs, for the stages whose stage data is read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StageIo {
    #[default]
    None,
    Vs {
        inputs: Vec<VsInput>,
        outputs: Vec<VsOutput>,
    },
    Ps {
        inputs: Vec<PsInput>,
        /// `SPI_PS_INPUT_ADDR`: which system values arrive in VGPRs.
        input_addr: u32,
        /// `DB_SHADER_CONTROL`: depth export, kill.
        db_shader_control: u32,
        patches: Vec<ExportPatch>,
    },
    Cs(ComputeIo),
    Ms(MeshIo),
}

/// A mesh program's outputs and limits. It runs as one hardware NGG stage:
/// lane n of the group (`threads` lanes) exports vertex n (`exp pos`,
/// `exp param`) and primitive n (`exp prim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshIo {
    /// `exp param<n>` targets; `param` bit 7 set: a per-primitive output,
    /// `row` then a row of the primitive signature (`PSG1`).
    pub outputs: Vec<VsOutput>,
    /// `PA_CL_VS_OUT_CNTL`: which clip and cull distances `pos1+` carry.
    pub clip_cntl: u32,
    /// Thread-group size in lanes (the HLSL group rounded up to waves).
    pub threads: u32,
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
    pub lds_bytes: u32,
}

/// A compute program's dispatch settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeIo {
    /// `COMPUTE_PGM_RSRC2`: user SGPRs, which system SGPRs and VGPRs
    /// follow them, LDS size.
    pub rsrc2: u32,
    /// Thread-group size (`numthreads`).
    pub threads: [u32; 3],
}

impl ComputeIo {
    /// User SGPRs before the system ones.
    pub fn user_sgprs(&self) -> u8 {
        (self.rsrc2 >> 1 & 0x1F) as u8
    }

    /// The SGPR of each enabled thread-group id component, in order after
    /// the user SGPRs, then the thread-group info SGPR if enabled.
    pub fn system_sgprs(&self) -> ([Option<u8>; 3], Option<u8>) {
        let mut next = self.user_sgprs();
        let mut take = |bit: u32| {
            (self.rsrc2 >> bit & 1 != 0).then(|| {
                next += 1;
                next - 1
            })
        };
        let ids = [take(7), take(8), take(9)];
        (ids, take(10))
    }

    /// SGPRs the hardware initializes: the user SGPRs, the enabled system
    /// ones, then the scratch wave offset when scratch is enabled (bit 0).
    /// The ones above hold whatever the previous wave left.
    pub fn initialized_sgprs(&self) -> u8 {
        let (ids, info) = self.system_sgprs();
        let end = ids.into_iter().chain([info]).flatten().map(|s| s + 1).max();
        end.unwrap_or(0).max(self.user_sgprs()) + (self.rsrc2 & 1) as u8
    }

    /// Thread id components in v0, v1, v2 (1 to 3). `TIDIG_COMP_CNT` is a
    /// 2-bit field, but a group id has only three components: 3 loads x, y
    /// and z like 2 does.
    pub fn thread_id_vgprs(&self) -> u8 {
        (self.rsrc2 >> 11 & 3).min(2) as u8 + 1
    }

    /// LDS bytes per thread group (512-byte granules).
    pub fn lds_bytes(&self) -> u32 {
        (self.rsrc2 >> 15 & 0x1FF) * 512
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RootType {
    Constants,
    /// A 64-bit buffer address.
    Descriptor,
    /// The 32-bit heap address of a descriptor table range.
    Table,
    /// A sampler the root signature fixes.
    ImmediateSampler,
    /// The 32-bit address of the CBV/SRV/UAV heap (SM 6.6 dynamic resources).
    ResourceHeap,
    /// The 32-bit address of the sampler heap.
    SamplerHeap,
    Other(u32),
}

impl RootType {
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => RootType::Constants,
            1 => RootType::Descriptor,
            3 => RootType::Table,
            4 => RootType::ImmediateSampler,
            5 => RootType::ResourceHeap,
            6 => RootType::SamplerHeap,
            n => RootType::Other(n),
        }
    }
}

/// One root parameter (or table range) and the user SGPRs holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootParam {
    pub root: RootType,
    /// Of the range within its table.
    pub table_offset_dwords: u32,
    /// User SGPRs from `user_data` on.
    pub sgprs: u32,
    /// Shader register of the first descriptor (`b`/`t`/`u`/`s` number).
    pub base_register: u32,
    /// First user-data dword ([`HwStage::user_sgpr`]).
    pub user_data: u8,
    /// Index of the parameter in the root signature.
    pub slot: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    Srv,
    Sampler,
    Cbv,
    Uav,
    Other(u8),
}

/// A descriptor range the program reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsedResource {
    pub kind: ResourceType,
    pub root: RootType,
    pub table_offset_dwords: u32,
    /// Shader register of the first descriptor read.
    pub lower_bound: u32,
    pub stride: u32,
    /// Size of the descriptor (4 or 8).
    pub dwords: u8,
    pub user_data: u8,
    pub slot: u8,
}

/// The native programs in a `DXIL` part's data; empty for the PC layout.
pub fn native_programs(part: &[u8]) -> Result<Vec<NativeProgram>, ShaderError> {
    if part.get(XDAT_OFFSET..XDAT_OFFSET + 4) != Some(b"XDAT") {
        return Ok(Vec::new());
    }
    let count = u32_at(part, XDAT_OFFSET + 4)? as usize;
    if count > MAX_XDAT_ENTRIES {
        return Err(Malformed("implausible XDAT entry count"));
    }
    let mut out = Vec::new();
    for i in 0..count {
        let at = XDAT_OFFSET + 12 + 12 * i;
        if u32_at(part, at)? != KIND_NATIVE {
            continue;
        }
        let offset = u32_at(part, at + 4)?.into();
        let size = u32_at(part, at + 8)?.into();
        let blob = bytes(part, offset, size).ok_or(Malformed("XDAT entry out of bounds"))?;
        out.extend(programs(&unpack(blob)?)?);
    }
    Ok(out)
}

fn unpack(blob: &[u8]) -> Result<Vec<u8>, ShaderError> {
    if blob.get(0..4) != Some(b"XBCH") {
        return Err(Malformed("native blob lacks XBCH magic"));
    }
    if u32_at(blob, 4)? != XBCH_VERSION {
        return Err(Malformed("unknown XBCH version"));
    }
    let packed = u64_at(blob, 8)?;
    let unpacked = u64_at(blob, 16)?;
    if unpacked > MAX_UNPACKED {
        return Err(Malformed("implausible XBCH unpacked size"));
    }
    let stream =
        bytes(blob, XBCH_HEADER as u64, packed).ok_or(Malformed("XBCH stream out of bounds"))?;
    let out = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(stream, unpacked as usize)
        .map_err(|_| Malformed("XBCH stream does not inflate to its stated size"))?;
    if out.len() as u64 != unpacked {
        return Err(Malformed("XBCH stream does not inflate to its stated size"));
    }
    Ok(out)
}

fn programs(fssm: &[u8]) -> Result<Vec<NativeProgram>, ShaderError> {
    if fssm.get(0..4) != Some(b"FSSM") {
        return Err(Malformed("native blob lacks FSSM magic"));
    }
    if u32_at(fssm, 4)? != FSSM_VERSION {
        return Err(Malformed("unknown FSSM version"));
    }
    let payload = &fssm[FSSM_HEADER.min(fssm.len())..];
    if u64_at(fssm, 8)? != payload.len() as u64 {
        return Err(Malformed("FSSM payload size disagrees"));
    }
    if u64_at(payload, 0)? != GRAPHICS_SLOT_COUNT {
        return Err(Malformed("unexpected FSSM slot count"));
    }
    let mut out = Vec::new();
    for (stage, at) in SLOTS {
        let start = u64_at(payload, at)?;
        if start == ABSENT {
            continue;
        }
        let record =
            bytes(payload, start, RECORD_SIZE).ok_or(Malformed("FSSM record out of bounds"))?;
        let code = bytes(payload, u64_at(record, 0)?, u64_at(record, 8)?)
            .ok_or(Malformed("native code out of bounds"))?;
        let vgprs = (u32_at(record, RECORD_REGS)? & 0x3FF) as u16;
        if vgprs > 256 {
            return Err(Malformed("implausible VGPR count"));
        }
        let table = |at: usize, count_at: usize| -> Result<&[u8], ShaderError> {
            let len = ENTRY_SIZE * u64::from(u32_at(record, count_at)?);
            if len == 0 {
                return Ok(&[]);
            }
            bytes(payload, u64_at(record, at)?, len)
                .ok_or(Malformed("FSSM root table out of bounds"))
        };
        let layout = table(RECORD_LAYOUT, RECORD_COUNTS)?
            .as_chunks::<{ ENTRY_SIZE as usize }>()
            .0
            .iter()
            .map(|e| RootParam {
                root: RootType::from_raw(le32(e, 0)),
                table_offset_dwords: le32(e, 4),
                sgprs: le32(e, 8),
                base_register: le32(e, 12),
                user_data: e[16],
                slot: e[17],
            })
            .collect::<Vec<RootParam>>();
        // A parameter's SGPRs must exist: `user_sgpr` and the high half of a
        // root descriptor are computed from these bytes ([`sgpr_span`]).
        if layout.iter().any(|p| {
            let (first, span) = sgpr_span(stage, p);
            u64::from(first) + u64::from(span) > u64::from(SGPRS)
        }) {
            return Err(Malformed("root parameter outside the SGPRs"));
        }
        // A table or heap parameter is one 32-bit address. Resource tracking
        // maps each SGPR it states to the parameter (`resource::param_of`),
        // and the host fills every dword of a table with that address
        // (`draw::user_value`; lowering reads a heap's as 0), so an SGPR past
        // the first would silently read the address too.
        if layout.iter().any(|p| {
            matches!(
                p.root,
                RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap
            ) && p.sgprs > 1
        }) {
            return Err(Malformed("root address wider than one SGPR"));
        }
        // Resource tracking maps an SGPR to the first parameter covering it
        // (`resource::param_of`), so a second parameter over the same SGPR
        // would have its reads attributed to the first. The ranges of one
        // table share its SGPR (and its address), so only different slots
        // are refused.
        if !params_apart(stage, &layout) {
            return Err(Malformed("root parameters share an SGPR"));
        }
        let used = table(RECORD_USED, RECORD_COUNTS + 4)?
            .as_chunks::<{ ENTRY_SIZE as usize }>()
            .0
            .iter()
            .map(|e| UsedResource {
                kind: match e[0] {
                    0 => ResourceType::Srv,
                    1 => ResourceType::Sampler,
                    2 => ResourceType::Cbv,
                    3 => ResourceType::Uav,
                    n => ResourceType::Other(n),
                },
                root: RootType::from_raw(e[1].into()),
                table_offset_dwords: le32(e, 4),
                lower_bound: le32(e, 8),
                stride: le32(e, 12),
                dwords: e[16],
                user_data: e[17],
                slot: e[18],
            })
            .collect::<Vec<UsedResource>>();
        // The host binds a table read by its root slot (`draw::tables`),
        // while lowering finds what a read is by the table address's SGPR
        // (`used_kind`). A read naming no table parameter with both would be
        // classified from one table and its descriptors bound from another,
        // or from none. A root descriptor's read is likewise found by its
        // SGPR (`handle`, which picks CBV or SRV from it) and bound by the
        // parameter's slot, so it must name that parameter with both too.
        let named = |u: &UsedResource| {
            layout
                .iter()
                .any(|p| p.root == u.root && p.slot == u.slot && p.user_data == u.user_data)
        };
        if used.iter().any(|u| u.root == RootType::Table && !named(u)) {
            return Err(Malformed("used descriptor names no root table"));
        }
        if used
            .iter()
            .any(|u| u.root == RootType::Descriptor && !named(u))
        {
            return Err(Malformed("used descriptor names no root descriptor"));
        }
        if !used_one_way(&used) {
            return Err(Malformed("used descriptors read one descriptor two ways"));
        }
        if !tables_one_heap(&used) {
            return Err(Malformed("root table mixes samplers and views"));
        }
        // The record is in bounds, so its end is too.
        let stage_data = &payload[(start + RECORD_SIZE) as usize..];
        let io = match stage {
            HwStage::Ps => ps_io(stage_data, code.len())?,
            HwStage::Vs => vs_outputs_per_vertex(vs_inputs_fit(vs_io(stage_data)?)?)?,
            HwStage::Cs => StageIo::Cs(compute_io(stage_data, &layout)?),
            HwStage::Ms => ms_io(stage_data, u32_at(record, RECORD_LDS)?)?,
            _ => StageIo::None,
        };
        out.push(NativeProgram {
            stage,
            wave64: u32_at(record, RECORD_FLAGS)? & FLAG_WAVE32 == 0,
            vgprs,
            layout,
            used,
            io,
            code: code.to_vec(),
        });
    }
    Ok(out)
}

/// A compute program's dispatch settings. `COMPUTE_PGM_RSRC2.LDS_SIZE` is a
/// 9-bit count of 512-byte granules, so it can state up to 255.5 KiB, which
/// lowering would declare as the group's shared memory; like a mesh group's,
/// a compute group's LDS can't exceed 64 KiB.
///
/// The group ids and group info arrive in the SGPRs after the user SGPRs
/// (`ComputeIo::system_sgprs`), and raising labels them there. A root
/// parameter covering one of those SGPRs would make the same register both a
/// root parameter and a group id, so such a layout is refused.
fn compute_io(data: &[u8], layout: &[RootParam]) -> Result<ComputeIo, ShaderError> {
    let cs = ComputeIo {
        rsrc2: u32_at(data, CS_RSRC2)?,
        threads: group_size(data)?,
    };
    if cs.lds_bytes() > MAX_LDS_BYTES {
        return Err(Malformed("implausible LDS size"));
    }
    let (ids, info) = cs.system_sgprs();
    // The layout was checked first, so `first + span` stays below `SGPRS`.
    let mut system = ids.into_iter().chain([info]).flatten().map(u32::from);
    if system.any(|s| {
        layout.iter().any(|p| {
            let (first, span) = sgpr_span(HwStage::Cs, p);
            (first..first + span).contains(&s)
        })
    }) {
        return Err(Malformed("root parameter over a system SGPR"));
    }
    Ok(cs)
}

/// The first SGPR of a root parameter and how many it takes. Resource
/// tracking reads a root descriptor's high half from the SGPR after its
/// first whatever the stated count, so a descriptor spans at least two.
fn sgpr_span(stage: HwStage, p: &RootParam) -> (u32, u32) {
    let first = u32::from(p.user_data) + u32::from(stage.user_sgpr(0));
    let span = match p.root {
        RootType::Descriptor => p.sgprs.max(2),
        _ => p.sgprs.max(1),
    };
    (first, span)
}

/// Whether no SGPR holds two root parameters: each covers its stated
/// SGPRs (none for, say, a fixed sampler), a root descriptor also the high
/// half resource tracking reads after its first. Only the ranges of one
/// table (one slot, one table address SGPR) may share; a slot number alone
/// does not make two entries one parameter, as a table range and constants
/// stating the same slot would still be read as whichever comes first.
fn params_apart(stage: HwStage, layout: &[RootParam]) -> bool {
    // The layout was checked first, so every covered SGPR is below `SGPRS`.
    let mut owner = [None; SGPRS as usize];
    layout.iter().all(|p| {
        let (first, _) = sgpr_span(stage, p);
        let span = match p.root {
            RootType::Descriptor => p.sgprs.max(2),
            _ => p.sgprs,
        };
        let key = (p.slot, p.root, first);
        (first..first + span).all(|s| match owner[s as usize].replace(key) {
            None => true,
            Some(held) => held == key && p.root == RootType::Table,
        })
    })
}

/// The descriptor a table or root-descriptor read names: its parameter and,
/// in a table, its offset. A root descriptor is one view, so all its reads
/// name the same one.
fn read_of(u: &UsedResource) -> Option<(RootType, u8, u8, u32)> {
    match u.root {
        RootType::Table => Some((u.root, u.slot, u.user_data, u.table_offset_dwords)),
        RootType::Descriptor => Some((u.root, u.slot, u.user_data, 0)),
        _ => None,
    }
}

/// Whether no descriptor is read as two kinds. Lowering classifies a read
/// by one entry (`used_kind`: a root descriptor's first, a table's last at
/// or below the offset) and picks CBV, SRV or UAV access from its kind,
/// while the host binds each entry by its own kind (`draw::readable`): a
/// second entry of another kind would bind the view one way and read it
/// another.
fn used_one_way(used: &[UsedResource]) -> bool {
    used.iter().enumerate().all(|(k, a)| {
        used[..k]
            .iter()
            .all(|b| read_of(a).is_none() || read_of(b) != read_of(a) || b.kind == a.kind)
    })
}

/// Whether each root table holds samplers only or views only. D3D12 puts
/// samplers in their own heap, so one table can't hold both, and the host
/// makes a table with a constant buffer afresh in its view heap
/// (`draw::tables`), where a sampler has no place.
fn tables_one_heap(used: &[UsedResource]) -> bool {
    let tables = || used.iter().filter(|u| u.root == RootType::Table);
    let sampler = |u: &UsedResource| u.kind == ResourceType::Sampler;
    tables().all(|a| {
        tables().all(|b| (a.slot, a.user_data) != (b.slot, b.user_data) || sampler(a) == sampler(b))
    })
}

/// A compute program's thread-group size. Lowering multiplies the
/// components and writes them as the DXIL `numthreads`, so each must be one
/// that HLSL allows: a zero or oversized group is refused here rather than
/// overflowing that product (a debug panic) or emitting an invalid shader.
fn group_size(data: &[u8]) -> Result<[u32; 3], ShaderError> {
    let mut threads = [0; 3];
    for (k, n) in threads.iter_mut().enumerate() {
        *n = u32_at(data, CS_THREADS + 4 * k)?;
    }
    // Each component is checked first, so the product fits a u64.
    if threads
        .iter()
        .zip(MAX_GROUP)
        .any(|(&n, max)| n == 0 || n > max)
        || threads.iter().map(|&n| u64::from(n)).product::<u64>() > MAX_GROUP_THREADS
    {
        return Err(Malformed("implausible thread-group size"));
    }
    Ok(threads)
}

/// The entries of `size` bytes from `at`, with their count at `count_at`.
fn semantics(data: &[u8], at: usize, size: usize, count_at: usize) -> Result<&[u8], ShaderError> {
    let count = u32_at(data, count_at)?;
    if count > MAX_SEMANTICS {
        return Err(Malformed("implausible semantic count"));
    }
    data.get(at..at + size * count as usize)
        .ok_or(Malformed("stage data out of bounds"))
}

fn ps_io(data: &[u8], code_len: usize) -> Result<StageIo, ShaderError> {
    let inputs = semantics(data, PS_INPUTS, 8, PS_INPUT_COUNT)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| PsInput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            mask: e[2] & 0xf,
            attr: e[3],
            flat: e[7] & 1 != 0,
            fp16: e[7] & 8 != 0,
            pack2: e[7] & 0x10 != 0,
            row2: e[5],
        })
        .collect::<Vec<_>>();
    // Raising resolves `v_interp` of attribute n to the first input naming
    // n, so a second input with the same attribute would be dropped.
    if !distinct(inputs.iter().map(|i| i.attr)) {
        return Err(Malformed("pixel inputs share an attribute"));
    }
    // Inputs packed into one signature row (disjoint channels) are fine, but
    // lowering finds a row's 16-bit layout as the first fp16 input naming
    // it: a second input on that row read as 32-bit, or packed differently,
    // would be decoded with the first's layout.
    let packing = |i: &PsInput| i.fp16.then_some((i.pack2, i.row2));
    if inputs.iter().enumerate().any(|(k, a)| {
        inputs[..k]
            .iter()
            .any(|b| b.row == a.row && packing(b) != packing(a))
    }) {
        return Err(Malformed("pixel inputs read a row two ways"));
    }
    // A packed input's halves past its own row's come from its second row
    // (`row2`), which lowering reads as that row's signature element
    // (`packed_half`). Another input naming that row, or a packed input of
    // another row claiming it too, would read one element as two inputs.
    let second = |i: &PsInput| (i.fp16 && i.pack2 && i.row2 != i.row).then_some(i.row2);
    if inputs.iter().any(|a| {
        second(a).is_some_and(|r2| {
            inputs
                .iter()
                .any(|b| b.row == r2 || (b.row != a.row && second(b) == Some(r2)))
        })
    }) {
        return Err(Malformed("pixel inputs claim a packed row"));
    }
    let size = u32_at(data, PS_PATCH_SIZE)? as usize;
    if size > PATCH_SLOT_DWORDS {
        return Err(Malformed("implausible export patch size"));
    }
    let mut patches = Vec::new();
    for mrt in 0..MRTS {
        let at = PS_PATCHES + mrt * PATCH_BYTES;
        let offset = u32_at(data, at)?;
        if offset == u32::MAX {
            continue;
        }
        if (offset as usize + size) * 4 > code_len {
            return Err(Malformed("export patch outside the code"));
        }
        // `patched_code` writes the patches in MRT order: a later one over an
        // earlier one's dwords would clobber that export with no error.
        let (start, end) = (offset as usize, offset as usize + size);
        if patches
            .iter()
            .any(|p: &ExportPatch| start < p.offset as usize + size && (p.offset as usize) < end)
        {
            return Err(Malformed("export patches overlap"));
        }
        let full = at + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
        let code = (0..size)
            .map(|i| u32_at(data, full + 4 * i))
            .collect::<Result<_, _>>()?;
        patches.push(ExportPatch {
            mrt: mrt as u8,
            offset,
            code,
        });
    }
    Ok(StageIo::Ps {
        inputs,
        input_addr: u32_at(data, PS_INPUT_ADDR)?,
        db_shader_control: u32_at(data, PS_DB_SHADER_CONTROL)?,
        patches,
    })
}

fn vs_io(data: &[u8]) -> Result<StageIo, ShaderError> {
    let inputs = semantics(data, VS_INPUTS, 5, VS_INPUT_COUNT)?
        .as_chunks::<5>()
        .0
        .iter()
        .map(|e| VsInput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            mask: e[2] & 0xf,
            vgpr: e[3],
        })
        .collect();
    let outputs = semantics(data, VS_OUTPUTS, 6, VS_OUTPUT_COUNT)?
        .as_chunks::<6>()
        .0
        .iter()
        .map(|e| VsOutput {
            usage: Usage::from_raw(e[0]),
            row: e[1],
            param: e[2],
            fp16: e[5] & 1 != 0,
            pack2: e[5] & 2 != 0,
        })
        .collect::<Vec<_>>();
    // Raising resolves `exp param<n>` to the first output whose parameter
    // (bit 7, a mesh program's per-primitive flag, aside) is n, so a second
    // output with the same parameter would be dropped.
    if !distinct(outputs.iter().map(|o| o.param & 0x7F)) {
        return Err(Malformed("vertex outputs share a parameter"));
    }
    // Raising turns each `exp param<n>` into a write of its output's
    // signature row, and lowering keys 16-bit outputs by that row
    // (`half_outputs`): two outputs naming one row would write the same
    // element, the later export silently replacing the earlier. Per-vertex
    // rows (`OSG1`) and per-primitive rows (`PSG1`) are separate signatures.
    let rows = |primitive: bool| {
        outputs
            .iter()
            .filter(move |o| (o.param & 0x80 != 0) == primitive)
            .map(|o| o.row)
    };
    if !distinct(rows(false)) || !distinct(rows(true)) {
        return Err(Malformed("vertex outputs share a row"));
    }
    Ok(StageIo::Vs { inputs, outputs })
}

/// Whether no byte appears twice.
fn distinct(bytes: impl IntoIterator<Item = u8>) -> bool {
    let mut seen = [false; 256];
    bytes
        .into_iter()
        .all(|b| !std::mem::replace(&mut seen[usize::from(b)], true))
}

/// The VGPRs a vertex program receives its vertex, primitive and instance
/// id in.
const VS_SYSTEM_VGPRS: [u32; 3] = [0, 2, 3];

/// Raising loads a vertex input's channel `c` from `v[vgpr + c]`, an 8-bit
/// register number: every enabled channel must land on a VGPR.
fn vs_inputs_fit(io: StageIo) -> Result<StageIo, ShaderError> {
    if let StageIo::Vs { inputs, .. } = &io
        && inputs.iter().any(|i| {
            i.mask
                .checked_ilog2()
                .is_some_and(|last| u32::from(i.vgpr) + last > u32::from(u8::MAX))
        })
    {
        return Err(Malformed("vertex input outside the VGPRs"));
    }
    // Raising also labels v0, v2 and v3 as the vertex, primitive and
    // instance id (`io::raise`) and takes the first label a register has, so
    // a channel loaded into one of them would silently read as that id.
    if let StageIo::Vs { inputs, .. } = &io
        && inputs.iter().any(|i| {
            (0..4)
                .filter(|c| i.mask >> c & 1 != 0)
                .any(|c| VS_SYSTEM_VGPRS.contains(&(u32::from(i.vgpr) + c)))
        })
    {
        return Err(Malformed("vertex input over a system VGPR"));
    }
    // Two inputs loading one VGPR would leave it labeled as the first; the
    // fetch would have overwritten it with the second, which raising drops.
    if let StageIo::Vs { inputs, .. } = &io {
        let mut taken = [false; 256];
        for i in inputs {
            for c in (0..4).filter(|c| i.mask >> c & 1 != 0) {
                // The first check keeps `vgpr + c` within v255.
                let r = usize::from(i.vgpr) + c;
                if std::mem::replace(&mut taken[r], true) {
                    return Err(Malformed("vertex inputs overlap"));
                }
            }
        }
    }
    Ok(io)
}

/// Bit 7 of an output's parameter marks a mesh program's per-primitive
/// output (a `PSG1` row). A vertex program has no primitive signature: such
/// an output's exports would be written to an element that doesn't exist
/// and silently dropped, and its row would escape the shared-row check.
fn vs_outputs_per_vertex(io: StageIo) -> Result<StageIo, ShaderError> {
    if let StageIo::Vs { outputs, .. } = &io
        && outputs.iter().any(|o| o.param & 0x80 != 0)
    {
        return Err(Malformed("vertex output marked per-primitive"));
    }
    Ok(io)
}

fn ms_io(data: &[u8], lds_bytes: u32) -> Result<StageIo, ShaderError> {
    let StageIo::Vs { outputs, .. } = vs_io(data)? else {
        unreachable!()
    };
    let info = data
        .get(MS_INFO..MS_INFO + 8)
        .ok_or(Malformed("stage data out of bounds"))?;
    let half = |at: usize| u32::from(u16::from_le_bytes([info[at], info[at + 1]]));
    // The group's lanes become the DXIL `numthreads` x and, for containers
    // without the HLSL count, the stride of the group id: an empty group
    // would emit `numthreads(0, 1, 1)` and give every group id 0.
    // A group past 128 lanes is one no mesh shader can declare (the lanes
    // are the HLSL group rounded up to waves, and 128 is a whole number of
    // either), so it would emit a `numthreads` the validator rejects.
    let threads = half(0);
    if threads == 0 || threads > MAX_MESH_THREADS {
        return Err(Malformed("implausible thread-group size"));
    }
    // Lowering writes the vertex and primitive limits and the topology as
    // the DXIL mesh state and the PSV mesh info as they are (the limits as
    // 16-bit values): past 256 outputs, or a topology neither lines (1) nor
    // triangles (2), the emitted shader is one D3D12 refuses to create.
    let (max_vertices, max_primitives) = (half(2), half(4));
    if max_vertices > MAX_MESH_OUTPUTS || max_primitives > MAX_MESH_OUTPUTS {
        return Err(Malformed("implausible mesh output count"));
    }
    let topology = info[7];
    if !matches!(topology, 1 | 2) {
        return Err(Malformed("unknown mesh topology"));
    }
    // Lowering declares the LDS as `lds_bytes.div_ceil(4)` dwords and states
    // it again in bytes (`lds_dwords * 4`), a u32 product that overflows for
    // sizes near `u32::MAX`; a group's LDS can't exceed 64 KiB anyway.
    if lds_bytes > MAX_LDS_BYTES {
        return Err(Malformed("implausible LDS size"));
    }
    Ok(StageIo::Ms(MeshIo {
        outputs,
        clip_cntl: u32_at(data, MS_CLIP_CNTL)?,
        threads,
        max_vertices,
        max_primitives,
        topology,
        lds_bytes,
    }))
}

/// For entries already sized by `as_chunks`.
fn le32(entry: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(entry[at..at + 4].try_into().unwrap())
}

/// `data[offset..offset + len]` with untrusted 64-bit bounds.
fn bytes(data: &[u8], offset: u64, len: u64) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    data.get(start..start.checked_add(usize::try_from(len).ok()?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VS_CODE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const PS_CODE: [u8; 4] = [9, 10, 11, 12];
    const VGPRS: u32 = 28;

    fn slot(stage: HwStage) -> usize {
        SLOTS.iter().find(|s| s.0 == stage).unwrap().1
    }

    /// Stage data: no semantics, and every export patch absent.
    const STAGE_DATA: usize = PS_PATCHES + MRTS * PATCH_BYTES;

    fn empty_stage_data() -> Vec<u8> {
        let mut data = vec![0; STAGE_DATA];
        for mrt in 0..MRTS {
            let at = PS_PATCHES + mrt * PATCH_BYTES;
            data[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        data
    }

    /// Where a one-program blob's stage data starts.
    fn stage_data_at(blob: &[u8], stage: HwStage) -> usize {
        FSSM_HEADER
            + u64_at(blob, FSSM_HEADER + slot(stage)).unwrap() as usize
            + RECORD_SIZE as usize
    }

    /// FSSM blob with `programs` as (stage, code) pairs.
    fn fssm(programs: &[(HwStage, &[u8])]) -> Vec<u8> {
        let mut payload = GRAPHICS_SLOT_COUNT.to_le_bytes().to_vec();
        payload.resize(slot(HwStage::Cs) + 8, 0);
        for (_, at) in SLOTS {
            payload[at..at + 8].copy_from_slice(&ABSENT.to_le_bytes());
        }
        for &(stage, code) in programs {
            let slot = slot(stage);
            let record = payload.len() as u64;
            payload[slot..slot + 8].copy_from_slice(&record.to_le_bytes());
            let code_at = record + RECORD_SIZE + STAGE_DATA as u64;
            payload.extend_from_slice(&code_at.to_le_bytes());
            payload.extend_from_slice(&(code.len() as u64).to_le_bytes());
            let flags = if stage == HwStage::Ps { FLAG_WAVE32 } else { 0 };
            payload.extend_from_slice(&[0; 4]);
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.resize(record as usize + RECORD_REGS, 0);
            payload.extend_from_slice(&(VGPRS | 31 << 20).to_le_bytes());
            let mut data = empty_stage_data();
            // numthreads(1, 1, 1): a compute or mesh group is never empty.
            if stage == HwStage::Cs {
                for k in 0..3 {
                    data[CS_THREADS + 4 * k..][..4].copy_from_slice(&1u32.to_le_bytes());
                }
            }
            // One lane drawing triangles: a mesh group is never empty and
            // always names a topology.
            if stage == HwStage::Ms {
                data[MS_INFO..][..2].copy_from_slice(&1u16.to_le_bytes());
                data[MS_INFO + 7] = 2;
            }
            payload.extend_from_slice(&data);
            payload.extend_from_slice(code);
        }
        let mut out = b"FSSM".to_vec();
        out.extend_from_slice(&FSSM_VERSION.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn xbch(unpacked: &[u8]) -> Vec<u8> {
        let stream = miniz_oxide::deflate::compress_to_vec_zlib(unpacked, 6);
        let mut out = b"XBCH".to_vec();
        out.extend_from_slice(&XBCH_VERSION.to_le_bytes());
        out.extend_from_slice(&(stream.len() as u64).to_le_bytes());
        out.extend_from_slice(&(unpacked.len() as u64).to_le_bytes());
        out.extend_from_slice(&stream);
        out
    }

    /// `DXIL` part data: program header, XDAT index, then the blobs.
    fn part(blobs: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = vec![0u8; XDAT_OFFSET];
        out.extend_from_slice(b"XDAT");
        out.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x24u32.to_le_bytes());
        let mut offset = out.len() + 12 * blobs.len();
        for (kind, blob) in blobs {
            for v in [*kind, offset as u32, blob.len() as u32] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            offset += blob.len();
        }
        for (_, blob) in blobs {
            out.extend_from_slice(blob);
        }
        out
    }

    #[test]
    fn finds_graphics_programs() {
        let native = xbch(&fssm(&[(HwStage::Vs, &VS_CODE), (HwStage::Ps, &PS_CODE)]));
        let data = part(&[(0x0a, b"xbop"), (KIND_NATIVE, &native)]);
        let found = native_programs(&data).unwrap();
        assert_eq!(
            found,
            [
                NativeProgram {
                    stage: HwStage::Vs,
                    wave64: true,
                    vgprs: 28,
                    layout: vec![],
                    used: vec![],
                    io: StageIo::Vs {
                        inputs: vec![],
                        outputs: vec![],
                    },
                    code: VS_CODE.to_vec()
                },
                NativeProgram {
                    stage: HwStage::Ps,
                    wave64: false,
                    vgprs: 28,
                    layout: vec![],
                    used: vec![],
                    io: StageIo::Ps {
                        inputs: vec![],
                        input_addr: 0,
                        db_shader_control: 0,
                        patches: vec![],
                    },
                    code: PS_CODE.to_vec()
                },
            ]
        );
    }

    #[test]
    fn reads_root_tables() {
        let mut blob = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let record = FSSM_HEADER + u64_at(&blob, FSSM_HEADER + slot(HwStage::Ps)).unwrap() as usize;
        let tables = (blob.len() - FSSM_HEADER) as u64;
        // A table range in s9 (root slot 1), and an SRV read from it.
        for v in [3u32, 2, 1, 5] {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob.extend_from_slice(&[9, 1, 0, 0, 0, 3, 0, 0]);
        for v in [2u32, 5, 4] {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob.extend_from_slice(&[8, 9, 1, 0]);
        let payload = (blob.len() - FSSM_HEADER) as u64;
        blob[8..16].copy_from_slice(&payload.to_le_bytes());
        blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&tables.to_le_bytes());
        blob[record + RECORD_USED..][..8].copy_from_slice(&(tables + ENTRY_SIZE).to_le_bytes());
        blob[record + RECORD_COUNTS..][..8].copy_from_slice(&[1, 0, 0, 0, 1, 0, 0, 0]);

        let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
        assert_eq!(
            p.layout,
            [RootParam {
                root: RootType::Table,
                table_offset_dwords: 2,
                sgprs: 1,
                base_register: 5,
                user_data: 9,
                slot: 1
            }]
        );
        assert_eq!(
            p.used,
            [UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Table,
                table_offset_dwords: 2,
                lower_bound: 5,
                stride: 4,
                dwords: 8,
                user_data: 9,
                slot: 1
            }]
        );

        // Parameters must sit in SGPRs the program can address: s9 plus 97
        // dwords ends at s105, one more is past it, and so is a user-data
        // byte past the SGPR file. (Constants: a table takes one SGPR, and
        // the read is then not a table's.)
        let entry = FSSM_HEADER + tables as usize;
        let layout = |blob: &[u8]| native_programs(&part(&[(KIND_NATIVE, &xbch(blob))]));
        let mut wide = blob.clone();
        wide[entry..entry + 4].copy_from_slice(&0u32.to_le_bytes());
        wide[entry + ENTRY_SIZE as usize + 1] = 0;
        wide[entry + 8..entry + 12].copy_from_slice(&97u32.to_le_bytes());
        assert_eq!(layout(&wide).unwrap()[0].layout[0].sgprs, 97);
        wide[entry + 8..entry + 12].copy_from_slice(&98u32.to_le_bytes());
        assert_eq!(
            refused_blob(&xbch(&wide)),
            "root parameter outside the SGPRs"
        );
        let mut far = blob.clone();
        far[entry + 16] = 0xFF;
        assert_eq!(
            refused_blob(&xbch(&far)),
            "root parameter outside the SGPRs"
        );

        // A root descriptor's high half is the SGPR after its first, even
        // when the record states fewer than two: one in s104 fits (s104,
        // s105), one in s105 would take VCC_LO as its high half.
        for sgprs in [0u32, 1, 2] {
            let mut desc = blob.clone();
            desc[entry..entry + 4].copy_from_slice(&1u32.to_le_bytes());
            // The SRV read through it is then a root descriptor's.
            desc[entry + ENTRY_SIZE as usize + 1] = 1;
            desc[entry + 8..entry + 12].copy_from_slice(&sgprs.to_le_bytes());
            desc[entry + 16] = 104;
            desc[entry + ENTRY_SIZE as usize + 17] = 104;
            assert_eq!(
                layout(&desc).unwrap()[0].layout[0].root,
                RootType::Descriptor
            );
            desc[entry + 16] = 105;
            desc[entry + ENTRY_SIZE as usize + 17] = 105;
            assert_eq!(
                refused_blob(&xbch(&desc)),
                "root parameter outside the SGPRs"
            );
            // Other kinds of parameter still take one SGPR there.
            desc[entry..entry + 4].copy_from_slice(&3u32.to_le_bytes());
            desc[entry + ENTRY_SIZE as usize + 1] = 3;
            if sgprs <= 1 {
                assert!(layout(&desc).is_ok());
            }
        }

        // The SRV is read through the table in s9, root slot 1: a read
        // naming another slot or SGPR names no table the layout has.
        let used = entry + ENTRY_SIZE as usize;
        for (at, byte) in [(17, 8u8), (17, 10), (18, 0), (18, 2)] {
            let mut stray = blob.clone();
            stray[used + at] = byte;
            assert_eq!(
                refused_blob(&xbch(&stray)),
                "used descriptor names no root table",
                "byte {at} = {byte}"
            );
        }
        // Nor may it name a parameter of another kind there.
        let mut constants = blob.clone();
        constants[entry..entry + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            refused_blob(&xbch(&constants)),
            "used descriptor names no root table"
        );
        // A read of another kind of root is not a table read.
        let mut desc = constants.clone();
        desc[used + 1] = 0;
        assert!(layout(&desc).is_ok());

        // A root descriptor read must name the root descriptor in its SGPR
        // and slot: not another SGPR or slot, nor a table or constants there.
        let mut root = blob.clone();
        root[entry..entry + 4].copy_from_slice(&1u32.to_le_bytes());
        root[entry + 8..entry + 12].copy_from_slice(&2u32.to_le_bytes());
        root[used + 1] = 1;
        assert!(layout(&root).is_ok());
        for (at, byte) in [(17, 8u8), (17, 10), (18, 0), (18, 2)] {
            let mut stray = root.clone();
            stray[used + at] = byte;
            assert_eq!(
                refused_blob(&xbch(&stray)),
                "used descriptor names no root descriptor",
                "byte {at} = {byte}"
            );
        }
        for kind in [0u32, 3] {
            let mut other = root.clone();
            other[entry..entry + 4].copy_from_slice(&kind.to_le_bytes());
            // A table takes one SGPR.
            other[entry + 8..entry + 12].copy_from_slice(&1u32.to_le_bytes());
            assert_eq!(
                refused_blob(&xbch(&other)),
                "used descriptor names no root descriptor",
                "parameter kind {kind}"
            );
        }

        blob[record + RECORD_COUNTS] = 9;
        assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).is_err());
    }

    #[test]
    fn reads_pixel_io_and_applies_the_full_export_patch() {
        let code = [0xBF80_0000u32; 4]; // s_nop
        let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut blob = fssm(&[(HwStage::Ps, &bytes)]);
        let e = stage_data_at(&blob, HwStage::Ps);
        // TEXCOORD in row 1 (xy), read as attr0, flat.
        blob[e + PS_INPUTS..][..8].copy_from_slice(&[6, 1, 3, 0, 0, 0, 1, 1]);
        blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&1u32.to_le_bytes());
        blob[e + PS_INPUT_ADDR..][..4].copy_from_slice(&2u32.to_le_bytes());
        blob[e + PS_PATCH_SIZE..][..4].copy_from_slice(&2u32.to_le_bytes());
        // MRT1 at dword 1: the 32_ABGR variant is `exp mrt1 v0-v3 done`.
        let patch = e + PS_PATCHES + PATCH_BYTES;
        blob[patch..][..4].copy_from_slice(&1u32.to_le_bytes());
        let full = patch + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
        blob[full..][..8].copy_from_slice(&[0x1f, 0x18, 0, 0xf8, 0, 1, 2, 3]);

        let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
        let StageIo::Ps {
            inputs,
            input_addr,
            patches,
            ..
        } = &p.io
        else {
            panic!("{:?}", p.io)
        };
        let input = PsInput {
            usage: Usage::Generic,
            row: 1,
            mask: 3,
            attr: 0,
            flat: true,
            fp16: false,
            pack2: false,
            row2: 0,
        };
        assert_eq!((&inputs[..], *input_addr), (&[input][..], 2));
        assert_eq!(patches.len(), 1);
        assert_eq!((patches[0].mrt, patches[0].offset), (1, 1));
        let patched = p.patched_code();
        assert_eq!(patched[..4], p.code[..4]);
        assert_eq!(patched[4..12], [0x1f, 0x18, 0, 0xf8, 0, 1, 2, 3]);
        assert_eq!(patched[12..], p.code[12..]);

        blob[patch..][..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).is_err());
    }

    #[test]
    fn finds_mesh_and_compute_programs() {
        for stage in [HwStage::Ms, HwStage::Cs] {
            let data = part(&[(KIND_NATIVE, &xbch(&fssm(&[(stage, &VS_CODE)])))]);
            let found = native_programs(&data).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].stage, stage);
        }
    }

    #[test]
    fn reads_mesh_outputs_and_limits() {
        let mut blob = fssm(&[(HwStage::Ms, &VS_CODE)]);
        let e = stage_data_at(&blob, HwStage::Ms);
        // param0 is row 1 per vertex, param2 row 0 of the primitive signature.
        blob[e + VS_OUTPUTS..][..12].copy_from_slice(&[6, 1, 0, 0, 0, 0, 6, 0, 0x82, 0, 0, 0]);
        blob[e + VS_OUTPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
        blob[e + MS_CLIP_CNTL..][..4].copy_from_slice(&0x0040_000Fu32.to_le_bytes());
        // 128 lanes, 128 vertices, 64 primitives, 13 user SGPRs, triangles.
        blob[e + MS_INFO..][..8].copy_from_slice(&[0x80, 0, 0x80, 0, 0x40, 0, 13, 2]);
        let record = e - RECORD_SIZE as usize;
        blob[record + RECORD_LDS..][..4].copy_from_slice(&16u32.to_le_bytes());
        let p = &native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).unwrap()[0];
        let out = |row, param| VsOutput {
            usage: Usage::Generic,
            row,
            param,
            fp16: false,
            pack2: false,
        };
        assert_eq!(
            p.io,
            StageIo::Ms(MeshIo {
                outputs: vec![out(1, 0), out(0, 0x82)],
                clip_cntl: 0x0040_000F,
                threads: 128,
                max_vertices: 128,
                max_primitives: 64,
                topology: 2,
                lds_bytes: 16,
            })
        );
    }

    #[test]
    fn compute_dispatch_settings() {
        // 5 user SGPRs, group id x and y, thread ids in v0-v1, 1 KiB LDS.
        let cs = ComputeIo {
            rsrc2: 0x1098A,
            threads: [16, 16, 1],
        };
        assert_eq!(cs.user_sgprs(), 5);
        assert_eq!(cs.system_sgprs(), ([Some(5), Some(6), None], None));
        assert_eq!((cs.thread_id_vgprs(), cs.lds_bytes()), (2, 1024));
        // 6 user SGPRs, all three group ids, v0-v2, no LDS.
        let cs = ComputeIo {
            rsrc2: 0x138C,
            threads: [8, 8, 1],
        };
        assert_eq!(cs.system_sgprs(), ([Some(6), Some(7), Some(8)], None));
        assert_eq!((cs.thread_id_vgprs(), cs.lds_bytes()), (3, 0));
    }

    #[test]
    fn initialized_sgprs_covers_user_system_and_scratch() {
        // 5 user SGPRs, group ids x and y (s5, s6), no scratch: s0..=s6.
        let cs = ComputeIo {
            rsrc2: 0x1098A,
            threads: [16, 16, 1],
        };
        assert_eq!(cs.initialized_sgprs(), 7);
        // All three group ids after 6 user SGPRs, plus the info SGPR: s0..=s9.
        let cs = ComputeIo {
            rsrc2: 0x78C,
            threads: [8, 8, 1],
        };
        assert_eq!(cs.initialized_sgprs(), 10);
        // The same with scratch enabled adds the wave offset: s0..=s10.
        let cs = ComputeIo {
            rsrc2: 0x78D,
            threads: [8, 8, 1],
        };
        assert_eq!(cs.initialized_sgprs(), 11);
    }

    #[test]
    fn compute_root_params_stay_off_system_sgprs() {
        // 4 user SGPRs, then group id x in s4, y in s5 and the group info
        // in s6.
        let rsrc2: u32 = 4 << 1 | 1 << 7 | 1 << 8 | 1 << 10;
        let program = |rsrc2: u32, root: u32, sgprs: u32, user_data: u8| {
            let mut blob = fssm(&[(HwStage::Cs, &VS_CODE)]);
            let record =
                FSSM_HEADER + u64_at(&blob, FSSM_HEADER + slot(HwStage::Cs)).unwrap() as usize;
            let e = record + RECORD_SIZE as usize;
            blob[e + CS_RSRC2..][..4].copy_from_slice(&rsrc2.to_le_bytes());
            let table = (blob.len() - FSSM_HEADER) as u64;
            for v in [root, 0, sgprs, 0] {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            blob.extend_from_slice(&[user_data, 0, 0, 0]);
            let payload = (blob.len() - FSSM_HEADER) as u64;
            blob[8..16].copy_from_slice(&payload.to_le_bytes());
            blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&table.to_le_bytes());
            blob[record + RECORD_COUNTS..][..8].copy_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        let (constants, descriptor, table) = (0, 1, 3);
        // s0-s3 are the user SGPRs; a table in s3 takes one of them.
        let p = program(rsrc2, constants, 4, 0).unwrap();
        assert_eq!(p[0].layout[0].sgprs, 4);
        assert!(program(rsrc2, table, 1, 3).is_ok());
        // Past the system SGPRs nothing collides.
        assert!(program(rsrc2, constants, 1, 7).is_ok());
        // With no group ids or info enabled, s4 is free.
        assert!(program(4 << 1, constants, 1, 4).is_ok());
        for (root, sgprs, user_data, what) in [
            (constants, 5, 0, "five dwords from s0 reach group id x"),
            (descriptor, 1, 3, "a descriptor's high half in s4"),
            (constants, 1, 5, "group id y"),
            (table, 1, 6, "the group info"),
        ] {
            assert!(
                matches!(
                    program(rsrc2, root, sgprs, user_data),
                    Err(Malformed("root parameter over a system SGPR"))
                ),
                "{what}"
            );
        }
    }

    #[test]
    fn thread_id_count_three_is_still_three_components() {
        // TIDIG_COMP_CNT (bits 11-12) of 3: still x, y and z in v0-v2,
        // never a fourth component in v3.
        let cs = ComputeIo {
            rsrc2: 3 << 11,
            threads: [4, 4, 4],
        };
        assert_eq!(cs.thread_id_vgprs(), 3);
    }

    #[test]
    fn reads_every_native_blob() {
        let es = xbch(&fssm(&[(HwStage::Es, &VS_CODE)]));
        let vs = xbch(&fssm(&[(HwStage::Vs, &VS_CODE)]));
        let data = part(&[(0x12, &[0; 16]), (KIND_NATIVE, &es), (KIND_NATIVE, &vs)]);
        let stages: Vec<_> = native_programs(&data)
            .unwrap()
            .iter()
            .map(|p| p.stage)
            .collect();
        assert_eq!(stages, [HwStage::Es, HwStage::Vs]);
    }

    #[test]
    fn pc_layout_has_none() {
        assert!(native_programs(&[0u8; 64]).unwrap().is_empty());
        assert!(
            native_programs(&part(&[(0x0a, b"xbop")]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_corrupt_blobs() {
        let good = xbch(&fssm(&[(HwStage::Ls, &VS_CODE)]));
        let corrupt = |at: usize, byte: u8| {
            let mut blob = good.clone();
            blob[at] = byte;
            native_programs(&part(&[(KIND_NATIVE, &blob)]))
        };
        assert!(corrupt(4, 9).is_err(), "unknown version");
        assert!(corrupt(16, 0xFF).is_err(), "stated size");
        assert!(corrupt(XBCH_HEADER + 2, 0xFF).is_err(), "zlib stream");
        assert!(native_programs(&part(&[(KIND_NATIVE, &good[..20])])).is_err());

        let mut bad_record = fssm(&[(HwStage::Ls, &VS_CODE)]);
        let at = FSSM_HEADER + slot(HwStage::Ls);
        bad_record[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&bad_record))])).is_err());
    }

    /// Why `data` (a `DXIL` part) is refused.
    fn refused(data: &[u8]) -> &'static str {
        match native_programs(data) {
            Err(Malformed(why)) => why,
            other => panic!("expected a malformed blob, got {other:?}"),
        }
    }

    /// Why a part holding `blob` as its only native entry is refused.
    fn refused_blob(blob: &[u8]) -> &'static str {
        refused(&part(&[(KIND_NATIVE, blob)]))
    }

    #[test]
    fn names_each_xdat_refusal() {
        let native = xbch(&fssm(&[(HwStage::Vs, &VS_CODE)]));
        let mut data = part(&[(KIND_NATIVE, &native)]);
        data[XDAT_OFFSET + 4..][..4].copy_from_slice(&(MAX_XDAT_ENTRIES as u32 + 1).to_le_bytes());
        assert_eq!(refused(&data), "implausible XDAT entry count");

        // A native entry whose size runs past the part.
        let mut data = part(&[(KIND_NATIVE, &native)]);
        let size = XDAT_OFFSET + 12 + 8;
        data[size..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(refused(&data), "XDAT entry out of bounds");

        // Entries of other kinds are never read, so their bounds do not matter.
        let mut data = part(&[(0x0e, b"tbl."), (KIND_NATIVE, &native)]);
        data[size..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(native_programs(&data).unwrap().len(), 1);
    }

    #[test]
    fn names_each_xbch_refusal() {
        let good = xbch(&fssm(&[(HwStage::Vs, &VS_CODE)]));
        let with = |at: usize, value: &[u8]| {
            let mut blob = good.clone();
            blob[at..at + value.len()].copy_from_slice(value);
            blob
        };
        assert_eq!(
            refused_blob(&with(0, b"XBCX")),
            "native blob lacks XBCH magic"
        );
        assert_eq!(
            refused_blob(&with(4, &(XBCH_VERSION + 1).to_le_bytes())),
            "unknown XBCH version"
        );
        assert_eq!(
            refused_blob(&with(16, &(MAX_UNPACKED + 1).to_le_bytes())),
            "implausible XBCH unpacked size"
        );
        let packed = u64_at(&good, 8).unwrap();
        assert_eq!(
            refused_blob(&with(8, &(packed + 1).to_le_bytes())),
            "XBCH stream out of bounds"
        );
        // A stated size either side of what the stream holds.
        let unpacked = u64_at(&good, 16).unwrap();
        for stated in [unpacked - 1, unpacked + 1] {
            assert_eq!(
                refused_blob(&with(16, &stated.to_le_bytes())),
                "XBCH stream does not inflate to its stated size"
            );
        }
        // A header with no stream after it.
        assert_eq!(refused_blob(&good[..20]), "truncated");
    }

    #[test]
    fn names_each_fssm_header_refusal() {
        let good = fssm(&[(HwStage::Vs, &VS_CODE)]);
        let with = |at: usize, value: &[u8]| {
            let mut blob = good.clone();
            blob[at..at + value.len()].copy_from_slice(value);
            xbch(&blob)
        };
        assert_eq!(
            refused_blob(&with(0, b"FSSX")),
            "native blob lacks FSSM magic"
        );
        assert_eq!(
            refused_blob(&with(4, &(FSSM_VERSION + 1).to_le_bytes())),
            "unknown FSSM version"
        );
        let payload = (good.len() - FSSM_HEADER) as u64;
        assert_eq!(
            refused_blob(&with(8, &(payload - 1).to_le_bytes())),
            "FSSM payload size disagrees"
        );
        assert_eq!(
            refused_blob(&with(FSSM_HEADER, &(GRAPHICS_SLOT_COUNT + 1).to_le_bytes())),
            "unexpected FSSM slot count"
        );
        assert_eq!(
            refused_blob(&with(
                FSSM_HEADER + slot(HwStage::Vs),
                &payload.to_le_bytes()
            )),
            "FSSM record out of bounds"
        );
    }

    #[test]
    fn names_each_record_refusal() {
        let good = fssm(&[(HwStage::Vs, &VS_CODE)]);
        let record = stage_data_at(&good, HwStage::Vs) - RECORD_SIZE as usize;
        let with = |at: usize, value: &[u8]| {
            let mut blob = good.clone();
            blob[at..at + value.len()].copy_from_slice(value);
            xbch(&blob)
        };
        // One byte more code than the payload holds.
        assert_eq!(
            refused_blob(&with(record + 8, &(VS_CODE.len() as u64 + 1).to_le_bytes())),
            "native code out of bounds"
        );
        assert_eq!(
            refused_blob(&with(record + RECORD_REGS, &257u32.to_le_bytes())),
            "implausible VGPR count"
        );
        // 256 is the most a program can have.
        let blob = with(record + RECORD_REGS, &256u32.to_le_bytes());
        assert_eq!(
            native_programs(&part(&[(KIND_NATIVE, &blob)])).unwrap()[0].vgprs,
            256
        );
        // One root parameter, at an offset past the payload.
        let mut blob = good.clone();
        blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&u64::MAX.to_le_bytes());
        blob[record + RECORD_COUNTS..][..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(refused_blob(&xbch(&blob)), "FSSM root table out of bounds");
    }

    #[test]
    fn names_each_stage_data_refusal() {
        let vs = fssm(&[(HwStage::Vs, &VS_CODE)]);
        let e = stage_data_at(&vs, HwStage::Vs);
        for count_at in [VS_INPUT_COUNT, VS_OUTPUT_COUNT] {
            let mut blob = vs.clone();
            blob[e + count_at..][..4].copy_from_slice(&(MAX_SEMANTICS + 1).to_le_bytes());
            assert_eq!(refused_blob(&xbch(&blob)), "implausible semantic count");
        }
        // One input in v252 or v253: x-w of v252 end at v255 and fit; from
        // v253, w would be v256.
        let input = |vgpr: u8, mask: u8| {
            let mut blob = vs.clone();
            blob[e + VS_INPUT_COUNT..][..4].copy_from_slice(&1u32.to_le_bytes());
            blob[e + VS_INPUTS..][..5].copy_from_slice(&[0, 0, mask, vgpr, 0]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        assert!(input(252, 0xf).is_ok());
        assert!(input(255, 0x1).is_ok());
        assert!(input(255, 0).is_ok(), "no channel is loaded");
        for (vgpr, mask) in [(253, 0xf), (255, 0x2)] {
            assert!(
                matches!(
                    input(vgpr, mask),
                    Err(Malformed("vertex input outside the VGPRs"))
                ),
                "v{vgpr} mask {mask:#x}"
            );
        }
        // v1 and v4 up hold no id: x in v1 and x-w from v4 load. Any channel
        // on v0, v2 or v3 is refused, including y and z of an input in v1.
        assert!(input(1, 0x1).is_ok());
        assert!(input(4, 0xf).is_ok());
        assert!(input(0, 0).is_ok(), "no channel is loaded");
        for (vgpr, mask) in [(0, 0x1), (1, 0x2), (2, 0x1), (3, 0x1), (1, 0x4)] {
            assert!(
                matches!(
                    input(vgpr, mask),
                    Err(Malformed("vertex input over a system VGPR"))
                ),
                "v{vgpr} mask {mask:#x}"
            );
        }
        // Two inputs: x-w in v4 and the second at `vgpr` with `mask`.
        let inputs = |vgpr: u8, mask: u8| {
            let mut blob = vs.clone();
            blob[e + VS_INPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
            blob[e + VS_INPUTS..][..10].copy_from_slice(&[0, 0, 0xf, 4, 0, 0, 1, mask, vgpr, 0]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        assert!(inputs(8, 0xf).is_ok());
        assert!(inputs(5, 0).is_ok(), "no channel is loaded");
        // x in v1 sits below the first input; any channel on v4-v7 overlaps
        // it, including w of an input in v1.
        assert!(inputs(1, 0x1).is_ok());
        for (vgpr, mask) in [(4, 0xf), (7, 0x1), (6, 0x2), (1, 0x8), (5, 0x4)] {
            assert!(
                matches!(inputs(vgpr, mask), Err(Malformed("vertex inputs overlap"))),
                "v{vgpr} mask {mask:#x}"
            );
        }

        let ps = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let e = stage_data_at(&ps, HwStage::Ps);
        let mut blob = ps.clone();
        blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&(MAX_SEMANTICS + 1).to_le_bytes());
        assert_eq!(refused_blob(&xbch(&blob)), "implausible semantic count");
        let mut blob = ps.clone();
        let size = PATCH_SLOT_DWORDS as u32 + 1;
        blob[e + PS_PATCH_SIZE..][..4].copy_from_slice(&size.to_le_bytes());
        assert_eq!(refused_blob(&xbch(&blob)), "implausible export patch size");
        // A one-dword patch at the code's last dword fits; one past it does not.
        let mut blob = ps.clone();
        blob[e + PS_PATCH_SIZE..][..4].copy_from_slice(&1u32.to_le_bytes());
        let last = (PS_CODE.len() / 4) as u32;
        blob[e + PS_PATCHES..][..4].copy_from_slice(&(last - 1).to_le_bytes());
        assert!(native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])).is_ok());
        blob[e + PS_PATCHES..][..4].copy_from_slice(&last.to_le_bytes());
        assert_eq!(refused_blob(&xbch(&blob)), "export patch outside the code");
        // Two-dword patches for MRT0 and MRT1: adjacent fits, sharing a
        // dword does not; with no dwords to patch, nothing can overlap.
        let code: Vec<u8> = [0xBF80_0000u32; 4] // s_nop
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let ps4 = fssm(&[(HwStage::Ps, &code)]);
        let patches = |size: u32, offsets: [u32; 2]| {
            let mut blob = ps4.clone();
            blob[e + PS_PATCH_SIZE..][..4].copy_from_slice(&size.to_le_bytes());
            for (mrt, offset) in offsets.into_iter().enumerate() {
                let at = e + PS_PATCHES + mrt * PATCH_BYTES;
                blob[at..][..4].copy_from_slice(&offset.to_le_bytes());
            }
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        assert!(patches(2, [0, 2]).is_ok());
        assert!(patches(2, [2, 0]).is_ok());
        assert!(patches(0, [1, 1]).is_ok());
        for offsets in [[0, 1], [1, 0], [1, 1]] {
            assert!(
                matches!(
                    patches(2, offsets),
                    Err(Malformed("export patches overlap"))
                ),
                "{offsets:?}"
            );
        }

        // numthreads at each limit fits; past one, or a zero component, does
        // not. 65536 x 65536 would overflow lowering's u32 thread count.
        let cs = fssm(&[(HwStage::Cs, &VS_CODE)]);
        let e = stage_data_at(&cs, HwStage::Cs);
        let group = |threads: [u32; 3]| {
            let mut blob = cs.clone();
            for (k, n) in threads.into_iter().enumerate() {
                blob[e + CS_THREADS + 4 * k..][..4].copy_from_slice(&n.to_le_bytes());
            }
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        for fits in [[1024, 1, 1], [1, 1024, 1], [1, 16, 64], [32, 32, 1]] {
            let p = group(fits).unwrap();
            assert!(
                matches!(p[0].io, StageIo::Cs(ComputeIo { threads, .. }) if threads == fits),
                "{fits:?}"
            );
        }
        for bad in [
            [0, 1, 1],
            [1, 1, 0],
            [1025, 1, 1],
            [1, 1, 65],
            [64, 32, 1],
            [65536, 65536, 1],
            [u32::MAX, u32::MAX, u32::MAX],
        ] {
            assert!(
                matches!(group(bad), Err(Malformed("implausible thread-group size"))),
                "{bad:?}"
            );
        }

        // A compute group's LDS (512-byte granules in RSRC2 bits 15-23):
        // 128 granules is 64 KiB and fits; one more, or the field's maximum
        // of 511, does not.
        let cs_lds = |granules: u32| {
            let mut blob = cs.clone();
            blob[e + CS_RSRC2..][..4].copy_from_slice(&(granules << 15).to_le_bytes());
            xbch(&blob)
        };
        let p = native_programs(&part(&[(KIND_NATIVE, &cs_lds(128))])).unwrap();
        assert!(matches!(p[0].io, StageIo::Cs(c) if c.lds_bytes() == MAX_LDS_BYTES));
        for bad in [129, 0x1FF] {
            assert_eq!(refused_blob(&cs_lds(bad)), "implausible LDS size", "{bad}");
        }

        // A mesh group of one lane fits; an empty one does not.
        let ms = fssm(&[(HwStage::Ms, &VS_CODE)]);
        let e = stage_data_at(&ms, HwStage::Ms);
        let p = native_programs(&part(&[(KIND_NATIVE, &xbch(&ms))])).unwrap();
        assert!(matches!(&p[0].io, StageIo::Ms(m) if m.threads == 1));
        let mut blob = ms.clone();
        blob[e + MS_INFO..][..2].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(refused_blob(&xbch(&blob)), "implausible thread-group size");

        // Mesh limits: 128 lanes and 256 vertices and primitives fit, with
        // lines or triangles; a lane, an output past them, or another
        // topology does not.
        let info = |bytes: [u8; 8]| {
            let mut blob = ms.clone();
            blob[e + MS_INFO..][..8].copy_from_slice(&bytes);
            xbch(&blob)
        };
        for topology in [1, 2] {
            let p = native_programs(&part(&[(
                KIND_NATIVE,
                &info([128, 0, 0, 1, 0, 1, 0, topology]),
            )]))
            .unwrap();
            assert!(matches!(
                &p[0].io,
                StageIo::Ms(m) if (m.threads, m.max_vertices, m.max_primitives, m.topology)
                    == (128, 256, 256, topology)
            ));
        }
        assert_eq!(
            refused_blob(&info([129, 0, 1, 0, 1, 0, 0, 2])),
            "implausible thread-group size"
        );
        assert_eq!(
            refused_blob(&info([0xff, 0xff, 1, 0, 1, 0, 0, 2])),
            "implausible thread-group size"
        );
        for bad in [
            [1, 0, 1, 1, 1, 0, 0, 2],
            [1, 0, 1, 0, 1, 1, 0, 2],
            [1, 0, 0xff, 0xff, 1, 0, 0, 2],
        ] {
            assert_eq!(
                refused_blob(&info(bad)),
                "implausible mesh output count",
                "{bad:?}"
            );
        }
        for topology in [0, 3, 0xff] {
            assert_eq!(
                refused_blob(&info([1, 0, 1, 0, 1, 0, 0, topology])),
                "unknown mesh topology",
                "{topology}"
            );
        }

        // A mesh group's LDS: 64 KiB fits; a byte more, or u32::MAX, does not.
        let lds = |bytes: u32| {
            let mut blob = ms.clone();
            let record = e - RECORD_SIZE as usize;
            blob[record + RECORD_LDS..][..4].copy_from_slice(&bytes.to_le_bytes());
            xbch(&blob)
        };
        let p = native_programs(&part(&[(KIND_NATIVE, &lds(MAX_LDS_BYTES))])).unwrap();
        assert!(matches!(&p[0].io, StageIo::Ms(m) if m.lds_bytes == 64 << 10));
        for bad in [MAX_LDS_BYTES + 1, u32::MAX] {
            assert_eq!(refused_blob(&lds(bad)), "implausible LDS size", "{bad}");
        }
    }

    #[test]
    fn refuses_outputs_sharing_a_parameter_and_inputs_sharing_an_attribute() {
        // Two outputs: param0 and `param`. Bit 7 (per-primitive) does not
        // make a separate index: `exp param<n>` matches n either way.
        for stage in [HwStage::Vs, HwStage::Ms] {
            let base = fssm(&[(stage, &VS_CODE)]);
            let e = stage_data_at(&base, stage);
            let outputs = |param: u8| {
                let mut blob = base.clone();
                blob[e + VS_OUTPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
                blob[e + VS_OUTPUTS..][..12]
                    .copy_from_slice(&[6, 0, 0, 0, 0, 0, 6, 1, param, 1, 0, 0]);
                native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
            };
            assert!(outputs(1).is_ok(), "{stage:?}");
            assert!(outputs(31).is_ok(), "{stage:?}");
            for param in [0, 0x80] {
                assert!(
                    matches!(
                        outputs(param),
                        Err(Malformed("vertex outputs share a parameter"))
                    ),
                    "{stage:?} param {param:#x}"
                );
            }
        }

        // Two inputs: attr0 and `attr`.
        let ps = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let e = stage_data_at(&ps, HwStage::Ps);
        let inputs = |attr: u8| {
            let mut blob = ps.clone();
            blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
            blob[e + PS_INPUTS..][..16]
                .copy_from_slice(&[6, 0, 0xf, 0, 0, 0, 0, 0, 6, 1, 0xf, attr, 0, 0, 1, 0]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        assert!(inputs(1).is_ok());
        assert!(matches!(
            inputs(0),
            Err(Malformed("pixel inputs share an attribute"))
        ));
    }

    #[test]
    fn refuses_pixel_inputs_reading_a_row_two_ways() {
        // Two inputs, attr0 and attr1, with these (row, row2, flags): flag
        // bit 3 is fp16, bit 4 two values per dword.
        let ps = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let e = stage_data_at(&ps, HwStage::Ps);
        let inputs = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            let mut blob = ps.clone();
            blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
            blob[e + PS_INPUTS..][..16].copy_from_slice(&[
                6, a.0, 0x3, 0, 0, a.1, 0, a.2, 6, b.0, 0xc, 1, 0, b.1, 1, b.2,
            ]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        let (fp16, pack2) = (0x8, 0x18);
        // Different rows may differ; one row packed the same way (or 32-bit
        // in both, or a 32-bit input's unused row2 differing) is fine.
        for (a, b) in [
            ((0, 0, 0), (1, 1, fp16)),
            ((0, 0, 0), (0, 0, 0)),
            ((0, 0, 0), (0, 5, 0)),
            ((2, 2, fp16), (2, 2, fp16)),
            ((2, 3, pack2), (2, 3, pack2)),
            ((0, 0, 0x1), (0, 0, 0x1)),
        ] {
            assert!(inputs(a, b).is_ok(), "{a:?} {b:?}");
        }
        for (a, b, what) in [
            ((0, 0, 0), (0, 0, fp16), "32-bit then fp16"),
            ((0, 0, fp16), (0, 0, 0), "fp16 then 32-bit"),
            ((2, 2, fp16), (2, 2, pack2), "one packed, one not"),
            ((2, 3, pack2), (2, 4, pack2), "different second rows"),
        ] {
            assert!(
                matches!(
                    inputs(a, b),
                    Err(Malformed("pixel inputs read a row two ways"))
                ),
                "{what}"
            );
        }
    }

    #[test]
    fn refuses_pixel_inputs_claiming_a_packed_row() {
        // Two inputs, attr0 and attr1, with these (row, row2, flags), as in
        // `refuses_pixel_inputs_reading_a_row_two_ways`.
        let ps = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let e = stage_data_at(&ps, HwStage::Ps);
        let inputs = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            let mut blob = ps.clone();
            blob[e + PS_INPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
            blob[e + PS_INPUTS..][..16].copy_from_slice(&[
                6, a.0, 0x3, 0, 0, a.1, 0, a.2, 6, b.0, 0xc, 1, 0, b.1, 1, b.2,
            ]);
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        let (fp16, pack2) = (0x8, 0x18);
        // A second row no input names; one row's inputs sharing theirs; a
        // packed input with no second row; an unpacked input's unused row2.
        for (a, b) in [
            ((2, 3, pack2), (4, 5, pack2)),
            ((2, 3, pack2), (2, 3, pack2)),
            ((2, 2, pack2), (3, 3, fp16)),
            ((2, 3, fp16), (3, 3, fp16)),
            ((2, 3, 0), (3, 3, 0)),
        ] {
            assert!(inputs(a, b).is_ok(), "{a:?} {b:?}");
        }
        for (a, b, what) in [
            ((2, 3, pack2), (3, 3, 0), "32-bit input on the second row"),
            ((2, 3, pack2), (3, 3, fp16), "fp16 input on the second row"),
            ((3, 3, fp16), (2, 3, pack2), "named before it is claimed"),
            ((2, 4, pack2), (3, 4, pack2), "two rows claiming one"),
            ((2, 3, pack2), (3, 2, pack2), "each claiming the other"),
        ] {
            assert!(
                matches!(
                    inputs(a, b),
                    Err(Malformed("pixel inputs claim a packed row"))
                ),
                "{what}"
            );
        }
    }

    #[test]
    fn refuses_outputs_sharing_a_row() {
        // Two outputs: param0 in row 0, param1 (or per-primitive param1) in
        // `row`.
        for stage in [HwStage::Vs, HwStage::Ms] {
            let base = fssm(&[(stage, &VS_CODE)]);
            let e = stage_data_at(&base, stage);
            let outputs = |param: u8, row: u8| {
                let mut blob = base.clone();
                blob[e + VS_OUTPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
                blob[e + VS_OUTPUTS..][..12]
                    .copy_from_slice(&[6, 0, 0, 0, 0, 0, 6, row, param, row, 0, 0]);
                native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
            };
            assert!(outputs(1, 1).is_ok(), "{stage:?}");
            // A per-primitive output's row is in the primitive signature, so
            // row 0 there is not the vertex output's row 0. Only a mesh
            // program has that signature.
            if stage == HwStage::Ms {
                assert!(outputs(0x81, 0).is_ok());
            } else {
                for (param, row) in [(0x81, 0), (0x81, 1), (0xFF, 7)] {
                    assert!(
                        matches!(
                            outputs(param, row),
                            Err(Malformed("vertex output marked per-primitive"))
                        ),
                        "{param:#x} in row {row}"
                    );
                }
            }
            assert!(
                matches!(outputs(1, 0), Err(Malformed("vertex outputs share a row"))),
                "{stage:?}"
            );
        }
        // Two per-primitive outputs in one row clash too.
        let base = fssm(&[(HwStage::Ms, &VS_CODE)]);
        let e = stage_data_at(&base, HwStage::Ms);
        let mut blob = base.clone();
        blob[e + VS_OUTPUT_COUNT..][..4].copy_from_slice(&2u32.to_le_bytes());
        blob[e + VS_OUTPUTS..][..12].copy_from_slice(&[6, 2, 0x80, 2, 0, 0, 6, 2, 0x81, 2, 0, 0]);
        assert!(matches!(
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))])),
            Err(Malformed("vertex outputs share a row"))
        ));
    }

    #[test]
    fn refuses_root_parameters_sharing_an_sgpr() {
        // A pixel program with these (root type, SGPRs, user data, slot)
        // parameters.
        let program = |params: &[(u32, u32, u8, u8)]| {
            let mut blob = fssm(&[(HwStage::Ps, &PS_CODE)]);
            let record =
                FSSM_HEADER + u64_at(&blob, FSSM_HEADER + slot(HwStage::Ps)).unwrap() as usize;
            let table = (blob.len() - FSSM_HEADER) as u64;
            for &(root, sgprs, user_data, slot) in params {
                for v in [root, 0, sgprs, 0] {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
                blob.extend_from_slice(&[user_data, slot, 0, 0]);
            }
            let payload = (blob.len() - FSSM_HEADER) as u64;
            blob[8..16].copy_from_slice(&payload.to_le_bytes());
            blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&table.to_le_bytes());
            let count = params.len() as u64;
            blob[record + RECORD_COUNTS..][..8].copy_from_slice(&count.to_le_bytes());
            native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
        };
        let (constants, descriptor, table, sampler) = (0, 1, 3, 4);
        // Side by side, two ranges of one table, and a fixed sampler (no
        // SGPRs) at a taken user-data dword are all fine.
        assert!(program(&[(constants, 2, 0, 0), (descriptor, 2, 2, 1)]).is_ok());
        assert!(program(&[(table, 1, 4, 2), (table, 1, 4, 2)]).is_ok());
        assert!(program(&[(table, 1, 4, 2), (table, 1, 4, 2), (table, 1, 4, 2)]).is_ok());
        assert!(program(&[(constants, 2, 0, 0), (sampler, 0, 0, 3)]).is_ok());
        for (params, what) in [
            (
                [(constants, 2, 0, 0), (table, 1, 1, 1)],
                "a table in the constants' second dword",
            ),
            (
                [(table, 1, 3, 0), (table, 1, 3, 1)],
                "two tables in one SGPR",
            ),
            (
                [(descriptor, 1, 0, 0), (constants, 1, 1, 1)],
                "constants in a descriptor's high half",
            ),
            (
                [(table, 1, 3, 2), (constants, 1, 3, 2)],
                "constants in a table's SGPR stating its slot",
            ),
            (
                [(constants, 2, 0, 0), (constants, 2, 1, 0)],
                "one slot's constants overlapping themselves",
            ),
            (
                [(constants, 1, 5, 4), (constants, 1, 5, 4)],
                "constants listed twice",
            ),
            (
                [(descriptor, 2, 0, 1), (descriptor, 2, 0, 1)],
                "a root descriptor listed twice",
            ),
        ] {
            assert_eq!(
                match program(&params) {
                    Err(Malformed(why)) => why,
                    other => panic!("{what}: expected a refusal, got {other:?}"),
                },
                "root parameters share an SGPR",
                "{what}"
            );
        }

        // A table or heap is one 32-bit address: it may state one SGPR (or
        // none, as a further range of a table), never two.
        let (resource_heap, sampler_heap) = (5, 6);
        for root in [table, resource_heap, sampler_heap] {
            for sgprs in [0, 1] {
                assert!(program(&[(root, sgprs, 4, 2)]).is_ok(), "{root}/{sgprs}");
            }
            for sgprs in [2, 8] {
                assert_eq!(
                    match program(&[(root, sgprs, 4, 2)]) {
                        Err(Malformed(why)) => why,
                        other => panic!("{root}/{sgprs}: expected a refusal, got {other:?}"),
                    },
                    "root address wider than one SGPR",
                    "{root}/{sgprs}"
                );
            }
        }
        // Constants and a root descriptor still take what they state.
        assert!(program(&[(constants, 8, 0, 0), (descriptor, 2, 8, 1)]).is_ok());
    }

    /// A pixel program with a table in s4 (slot 2), a root descriptor in
    /// s6-s7 (slot 3), and these (kind, root type, table offset) reads.
    fn reading(reads: &[(u8, u8, u32)]) -> Result<Vec<NativeProgram>, ShaderError> {
        let mut blob = fssm(&[(HwStage::Ps, &PS_CODE)]);
        let record = FSSM_HEADER + u64_at(&blob, FSSM_HEADER + slot(HwStage::Ps)).unwrap() as usize;
        let tables = (blob.len() - FSSM_HEADER) as u64;
        for (root, sgprs, user_data, slot) in [(3u32, 1u32, 4u8, 2u8), (1, 2, 6, 3)] {
            for v in [root, 0, sgprs, 0] {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            blob.extend_from_slice(&[user_data, slot, 0, 0]);
        }
        for &(kind, root, offset) in reads {
            let (user_data, slot) = if root == 3 { (4, 2) } else { (6, 3) };
            blob.extend_from_slice(&[kind, root, 0, 0]);
            for v in [offset, 0, 0] {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            blob.extend_from_slice(&[8, user_data, slot, 0]);
        }
        let payload = (blob.len() - FSSM_HEADER) as u64;
        blob[8..16].copy_from_slice(&payload.to_le_bytes());
        blob[record + RECORD_LAYOUT..][..8].copy_from_slice(&tables.to_le_bytes());
        let used = tables + 2 * ENTRY_SIZE;
        blob[record + RECORD_USED..][..8].copy_from_slice(&used.to_le_bytes());
        blob[record + RECORD_COUNTS..][..4].copy_from_slice(&2u32.to_le_bytes());
        blob[record + RECORD_COUNTS + 4..][..4]
            .copy_from_slice(&(reads.len() as u32).to_le_bytes());
        native_programs(&part(&[(KIND_NATIVE, &xbch(&blob))]))
    }

    #[test]
    fn refuses_a_descriptor_read_two_ways() {
        let (srv, cbv, uav) = (0u8, 2u8, 3u8);
        let (descriptor, table) = (1u8, 3u8);
        // Distinct table entries, the same entry listed twice as one kind,
        // and a root descriptor read twice as one kind are fine.
        assert!(reading(&[(srv, table, 0), (uav, table, 8), (cbv, table, 16)]).is_ok());
        assert!(reading(&[(srv, table, 8), (srv, table, 8)]).is_ok());
        assert!(reading(&[(cbv, descriptor, 0), (cbv, descriptor, 0)]).is_ok());
        for (reads, what) in [
            (
                [(srv, table, 8), (uav, table, 8)],
                "an SRV and a UAV at one offset",
            ),
            (
                [(cbv, table, 0), (srv, table, 0)],
                "a CBV and an SRV at one offset",
            ),
            (
                [(cbv, descriptor, 0), (srv, descriptor, 0)],
                "a root descriptor as a CBV and an SRV",
            ),
            (
                [(srv, descriptor, 0), (uav, descriptor, 4)],
                "a root descriptor at two offsets",
            ),
        ] {
            assert_eq!(
                why_refused(reading(&reads)),
                "used descriptors read one descriptor two ways",
                "{what}"
            );
        }
        // A table and a root descriptor are different descriptors even at
        // the same stated offset.
        assert!(reading(&[(srv, table, 0), (cbv, descriptor, 0)]).is_ok());
    }

    #[test]
    fn refuses_a_table_mixing_samplers_and_views() {
        let (srv, sampler, cbv) = (0u8, 1u8, 2u8);
        let table = 3u8;
        assert!(reading(&[(sampler, table, 0), (sampler, table, 4)]).is_ok());
        assert!(reading(&[(srv, table, 0), (cbv, table, 8)]).is_ok());
        for reads in [
            [(sampler, table, 0), (srv, table, 8)],
            [(cbv, table, 0), (sampler, table, 8)],
        ] {
            assert_eq!(
                why_refused(reading(&reads)),
                "root table mixes samplers and views",
                "{reads:?}"
            );
        }
    }

    fn why_refused(r: Result<Vec<NativeProgram>, ShaderError>) -> &'static str {
        match r {
            Err(Malformed(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
