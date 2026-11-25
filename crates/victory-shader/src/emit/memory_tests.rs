//! Title memory at addresses the program computes (decision 0005, the
//! unified-memory item), run on the host GPU and WARP with the test host
//! serving the contract (`emit` "Title memory"): a synthetic GFX10 program
//! takes a 64-bit pointer from root constants, reads through it with a
//! global and a scalar load, follows a pointer it loaded with another
//! scalar load, stores through a pointer it computes (64-bit multiply-add)
//! and does float min/max atomics, reads through the address it takes
//! from a buffer view's descriptor (the host's view addresses), reads
//! pointers into memory no copy holds (zeros, marked missed) and stores and
//! loads through a root UAV (title memory in such a program). Also: buffer
//! loads of a table in the program's code, zeros out of the records.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const S_WAITCNT_ALL: u32 = 0xBF8C_0000;
/// Inline constant 0, 64.
const ZERO: u16 = 128;
const SIXTY_FOUR: u16 = 192;
/// The title address of the memory the program reads: above 4 GiB.
const TITLE: u64 = 0x12_3450_0000;

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SMEM `s_load_dword*` (`op` 0, 1, 2: one, two, four dwords) at
/// `s[base:base+1] + offset`.
const fn s_load(op: u32, sdata: u8, sbase: u8, offset: u32) -> [u32; 2] {
    [
        0x3D << 26 | op << 18 | (sdata as u32) << 6 | (sbase as u32 >> 1),
        125 << 25 | offset,
    ]
}

/// VOP1 `v_mov_b32 vdst, src0`.
const fn v_mov_b32(vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | 0x01 << 9 | (src0 as u32 & 0x1FF)
}

/// GLOBAL `op` with address `s[saddr:saddr+1] + vaddr + offset`.
const fn global(op: u32, vdst: u8, vaddr: u8, data: u8, saddr: u8, offset: u32) -> [u32; 2] {
    [
        0x37 << 26 | op << 18 | 2 << 14 | (offset & 0xFFF),
        vaddr as u32 | (data as u32) << 8 | (saddr as u32) << 16 | (vdst as u32) << 24,
    ]
}

/// MUBUF `buffer_store_dword vdata, off, s[srsrc:srsrc+3], 0 offset:byte_off`.
const fn buffer_store_dword(vdata: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
    ]
}

fn program() -> HostShader {
    let mut w = vec![s_mov_b32(5, 0x80)]; // s[4:5]: the table at s4
    w.extend(s_load(2, 8, 4, 0)); // s[8:11]: the result UAV
    w.push(v_mov_b32(1, ZERO));
    w.extend(global(12, 2, 1, 0, 0, 0)); // global_load_dword v2, v1, s[0:1]
    w.extend(s_load(1, 12, 0, 8)); // s[12:13] = the pointer at +8
    w.push(S_WAITCNT_ALL);
    w.extend(s_load(0, 14, 12, 0)); // s14 = what it points at
    w.push(v_mov_b32(3, SIXTY_FOUR));
    w.push(v_mov_b32(4, 2)); // v4 = s2
    // v_mad_u64_u32 v[8:9], s[20:21], v3, 1, s[0:1]: the pointer plus 0x40,
    // stored through as a VGPR pair.
    w.extend([0xD576_1408, 0x0001_0303]);
    w.extend(global(28, 0, 8, 4, 0x7D, 0)); // global_store_dword v[8:9], v4, off
    w.push(v_mov_b32(6, 3)); // v6 = s3 (a float)
    w.extend(global(63, 0, 3, 6, 0, 0x40)); // global_atomic_fmin at +0x80
    w.extend(global(64, 0, 3, 6, 0, 0x44)); // global_atomic_fmax at +0x84
    // The address of the view at heap index 1 from its descriptor (as the
    // tracers take their BVH's), read through.
    w.extend(s_load(1, 16, 4, 0x20)); // s[16:17] = descriptor 1, dwords 0-1
    w.push(S_WAITCNT_ALL);
    w.extend(s_load(0, 18, 16, 0x100)); // s18 = the dword at +0x100
    w.push(S_WAITCNT_ALL);
    // Pointers into a page no copy holds and into a 256 MiB region with no
    // block, read through: zeros, and both marked missed.
    w.extend(s_load(1, 22, 0, 0x10)); // s[22:23]
    w.extend(s_load(1, 24, 0, 0x18)); // s[24:25]
    w.push(S_WAITCNT_ALL);
    w.extend(s_load(0, 26, 22, 0));
    w.extend(s_load(0, 27, 24, 0));
    w.push(S_WAITCNT_ALL);
    w.push(v_mov_b32(5, 14));
    w.push(v_mov_b32(7, 18));
    w.extend(buffer_store_dword(2, 8, 0));
    w.extend(buffer_store_dword(5, 8, 4));
    w.extend(buffer_store_dword(7, 8, 8));
    w.push(v_mov_b32(5, 26));
    w.push(v_mov_b32(7, 27));
    w.extend(buffer_store_dword(5, 8, 12));
    w.extend(buffer_store_dword(7, 8, 16));
    // A root UAV (s[6:7]), title memory in such a program: a store and a
    // load at its address.
    w.extend(global(28, 0, 1, 4, 6, 0x20)); // global_store_dword v1, v4, s[6:7]
    w.extend(global(12, 9, 1, 0, 6, 0x24)); // global_load_dword v9, v1, s[6:7]
    w.push(S_WAITCNT_ALL);
    w.extend(buffer_store_dword(9, 8, 20));
    w.push(S_ENDPGM);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 10,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
            RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 6,
                slot: 2,
            },
        ],
        used: vec![
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 2,
                user_data: 6,
                slot: 2,
            },
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 8 << 1,
            threads: [1, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let memory = res.scalar.iter().filter(|s| s.1.param == resource::MEMORY).count();
    assert_eq!(memory, 11, "every access through the pointer is title memory");
    let io = io::raise(&p, &native).expect("io::raise");
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .expect("lower");
    assert!(l.memory && l.memory_writes && l.view_addresses);
    assert!(l.root_uavs.is_empty(), "the root UAV is read as title memory");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).expect("DXIL validation failed");
    }
    shader
}

/// The page table for one copy of title memory at [`TITLE`] whose views
/// are at heap index `view` (null pair at `null`).
fn page_table(view: u32, null: u32) -> Vec<u32> {
    let mut t = vec![0u32; 4096];
    let (hi, lo) = ((TITLE >> 32) as u32, TITLE as u32);
    t[(hi << 4 | lo >> 28) as usize] = 1;
    for _ in 0..2 * 4096 {
        t.extend([null, 0]);
    }
    let entry = 4096 + 2 * (4096 + (lo >> 16 & 0xFFF) as usize);
    t[entry..entry + 2].copy_from_slice(&[view, lo]);
    t
}

#[test]
fn title_memory_is_read_and_written_at_computed_addresses() {
    let shader = program();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut memory = vec![0u32; 256];
    memory[0] = 0x1111_1111;
    let pointer = TITLE + 0x100;
    memory[2] = pointer as u32;
    memory[3] = (pointer >> 32) as u32;
    // The next page (no copy) and another 256 MiB region (no block).
    let (next, far) = (TITLE + 0x1_0010, 0x55_0000_0040u64);
    memory[4..8].copy_from_slice(&[next as u32, (next >> 32) as u32, far as u32, (far >> 32) as u32]);
    memory[0x40] = 0x2222_2222;
    // The root UAV's address, and what it loads there.
    let root_uav = TITLE + 0x200;
    memory[(0x200 + 0x24) / 4] = 0x3333_3333;
    memory[0x80 / 4] = 1.0f32.to_bits();
    memory[0x84 / 4] = (-1.0f32).to_bits();
    let value = 0xCAFE_F00D;
    // Heap index 1 (the memory's view) has the title address.
    let mut view_addresses = vec![0u32; 2 * 5];
    view_addresses[2..4].copy_from_slice(&[TITLE as u32, (TITLE >> 32) as u32]);
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        // fmin with 1.0 and fmax with -1.0 in memory, against a negative
        // and a positive operand.
        for operand in [-2.0f32, 0.5] {
            let mut ud = vec![0u32; shader.user_data_dwords() as usize];
            for u in &shader.user_data {
                let constants = [TITLE as u32, (TITLE >> 32) as u32, value, operand.to_bits()];
                ud[u.index as usize] = match u.root {
                    RootType::Constants => constants[u.dword as usize],
                    RootType::Descriptor => (root_uav >> (32 * u.dword)) as u32,
                    _ => 0,
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (1, Root::View { srv: true, data: page_table(1, 3) }),
                            (2, Root::Uav(2 * 4096 / 32)),
                            (3, Root::View { srv: true, data: view_addresses.clone() }),
                        ],
                        views: &[
                            View::RawUav(6),
                            View::Memory(&memory),
                            View::Pair,
                            View::NullPair,
                            View::Pair,
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(
                out[0],
                [0x1111_1111, 0x2222_2222, 0x2222_2222, 0, 0, 0x3333_3333],
                "{label}: reads"
            );
            assert_eq!(out[1][0x40 / 4], value, "{label}: the store");
            assert_eq!(out[1][(0x200 + 0x20) / 4], value, "{label}: the root UAV store");
            let floats = [f32::from_bits(out[1][0x20]), f32::from_bits(out[1][0x21])];
            let want = [operand.min(1.0), operand.max(-1.0)];
            assert_eq!(floats, want, "{label}: float min and max");
            // The far region by its level-1 index, the page written, the
            // page missed after it; nothing else.
            let entry = 4096 + (TITLE as u32 >> 16 & 0xFFF) as usize;
            let marked: Vec<usize> = (0..out[2].len() * 32)
                .filter(|&b| out[2][b / 32] >> (b % 32) & 1 != 0)
                .collect();
            assert_eq!(marked, [(far >> 28) as usize, entry, entry + 1], "{label}: pages");
        }
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

/// A root SRV in a program that does not use title memory is read through
/// the host's view of its host copy (`emit`, "Root SRVs and root UAVs"): the
/// view's index and the root address's offset in it come as user data, and
/// the shader adds the offset. Lanes read 64 bytes apart from the root
/// address 16 bytes into a 1 KiB copy: those inside it read the copy, those
/// past its end read 0 (a host root descriptor read whatever lay there).
#[test]
fn root_views_read_their_copy_and_zeros_past_it() {
    const fn vop2(op: u32, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
        op << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | src0 as u32
    }
    /// MUBUF `op` (offen) of `vdata` at `vaddr` through `s[srsrc:srsrc+3]`.
    const fn mubuf(op: u32, vdata: u8, vaddr: u8, srsrc: u8) -> [u32; 2] {
        [0x38 << 26 | op << 18 | 1 << 12, 0x80 << 24 | (srsrc as u32 >> 2) << 16 | (vdata as u32) << 8 | vaddr as u32]
    }
    const V_LSHLREV_B32: u32 = 0x1A;
    let mut w = vec![
        s_mov_b32(5, ZERO as u8), // s[4:5]: the table at s4
        s_mov_b32(0, 6),          // s[0:3]: the root SRV's buffer descriptor
        0x8801_FF07,
        0x0001_0000, // s_or_b32 s1, s7, stride 1
        0xBE82_03C1, // s_mov_b32 s2, -1
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
    ];
    w.extend(s_load(2, 8, 4, 0)); // s[8:11]: the result UAV
    w.push(vop2(V_LSHLREV_B32, 1, 128 + 6, 0)); // v1 = lane << 6
    w.extend(mubuf(0x0C, 2, 1, 0)); // buffer_load_dword v2, v1, s[0:3] offen
    w.push(vop2(V_LSHLREV_B32, 3, 128 + 2, 0)); // v3 = lane << 2
    w.push(S_WAITCNT_ALL);
    w.extend(mubuf(0x1C, 2, 3, 8)); // buffer_store_dword v2, v3, s[8:11] offen
    w.push(S_ENDPGM);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
            RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 6,
                slot: 2,
            },
        ],
        used: vec![
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 1,
                dwords: 4,
                user_data: 6,
                slot: 2,
            },
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 8 << 1,
            threads: [32, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .expect("lower");
    assert!(l.root_srvs.is_empty() && !l.memory, "no root descriptor, no title memory");
    let marks: Vec<(u32, u32)> = l
        .user_data
        .iter()
        .filter(|u| u.root == RootType::Descriptor)
        .map(|u| (u.index, u.dword))
        .collect();
    assert_eq!(marks, [(6, super::ROOT_SRV_VIEW), (7, super::ROOT_VIEW_OFFSET)]);
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let copy: Vec<u32> = (0..256).map(|k| 0x1000 + k).collect();
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        let mut ud = vec![0u32; shader.user_data_dwords() as usize];
        // The table at heap index 0, the copy's view at 1, the root address
        // 16 bytes into it.
        (ud[4], ud[6], ud[7]) = (0, 1, 16);
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[
                        View::RawUav(32),
                        View::Buffer {
                            data: &copy,
                            stride: super::host::Stride::Raw,
                        },
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let want: Vec<u32> = (0..32).map(|lane| if lane < 16 { 0x1000 + 4 + 16 * lane } else { 0 }).collect();
        assert_eq!(out[0], want, "{label}");
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

/// Writes through unordered buffer views mark their pages (`emit`, "Write
/// marks"): a dword store and a 16-byte store through view 0 (base bit 100,
/// its first byte 16 bytes before a 64 KiB page ends, so the second store
/// crosses into the next page), a store through view 1 (not marked) and an
/// atomic add through view 2 (base bit 200). The stores land; exactly bits
/// 100, 101 and 200 are set.
#[test]
fn writes_through_views_mark_their_pages() {
    /// MUBUF `op` of `vdata` at `byte_off` through `s[srsrc:srsrc+3]`.
    const fn mubuf(op: u32, vdata: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
        [
            0x38 << 26 | op << 18 | (byte_off as u32 & 0xFFF),
            0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
        ]
    }
    const BUFFER_STORE_DWORDX4: u32 = 0x1E;
    const BUFFER_ATOMIC_ADD: u32 = 0x32;
    let mut w = vec![s_mov_b32(5, ZERO as u8)];
    for (k, sdata) in [8u8, 12, 16].into_iter().enumerate() {
        w.extend(s_load(2, sdata, 4, 32 * k as u32));
    }
    w.push(S_WAITCNT_ALL);
    w.extend((0..4).map(|v| v_mov_b32(v, 0)));
    w.extend(buffer_store_dword(0, 8, 0));
    w.extend(mubuf(BUFFER_STORE_DWORDX4, 0, 8, 12));
    w.extend(buffer_store_dword(0, 12, 4));
    w.extend(mubuf(BUFFER_ATOMIC_ADD, 0, 16, 8));
    w.push(S_ENDPGM);
    let param = |root, user_data, sgprs, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let uav = |k: u32| UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 8 * k,
        lower_bound: k,
        stride: 0,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![param(RootType::Constants, 0, 4, 0), param(RootType::Table, 4, 1, 1)],
        used: (0..3).map(uav).collect(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: true,
    })
    .expect("lower");
    assert!(l.view_marks && !l.memory, "marks without the page table");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let value = 0x1234_5678;
    let marks = vec![100, 0xFFF0, super::VIEW_MARK_NONE, 0, 200, 0];
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        let mut ud = vec![0u32; shader.user_data_dwords() as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = if u.root == RootType::Constants { value } else { 0 };
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud)),
                        (1, Root::View { srv: true, data: marks.clone() }),
                        (2, Root::Uav(8)),
                    ],
                    views: &[View::RawUav(8), View::RawUav(8), View::RawUav(8)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(out[0], [value, 0, 0, value, value, value, value, 0], "{label}: view 0");
        assert_eq!(out[1][1], value, "{label}: view 1");
        assert_eq!(out[2][2], value, "{label}: the atomic");
        let marked: Vec<usize> = (0..out[3].len() * 32)
            .filter(|&b| out[3][b / 32] >> (b % 32) & 1 != 0)
            .collect();
        assert_eq!(marked, [100, 101, 200], "{label}: pages");
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

const OFFEN: u32 = 1 << 12;
const IDXEN: u32 = 1 << 13;

/// MUBUF `op vdata, vaddr, s[srsrc:srsrc+3], soffset`; `low` holds the
/// immediate offset and [`OFFEN`] or [`IDXEN`].
const fn mubuf(op: u32, vdata: u8, vaddr: u8, srsrc: u8, soffset: u8, low: u32) -> [u32; 2] {
    [
        0x38 << 26 | op << 18 | low,
        (soffset as u32) << 24 | (srsrc as u32 >> 2) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// The table [`code_table_program`] appends to its code.
fn code_table() -> Vec<u32> {
    (1..=64u32).map(|k| k.wrapping_mul(0x9E37_79B9)).collect()
}

/// Record limits of [`code_table_program`]'s two descriptors: bytes of the
/// raw one (lane 29's dword past it, its byte 1 not), elements of the
/// indexed one.
const RAW_RECORDS: u32 = 4 * 29 + 2;
const INDEXED_RECORDS: u32 = 21;

/// A wave32 program of 32 lanes reading a table after its code through two
/// descriptors built around `s_getpc_b64`: a raw one (`OOB_SELECT` 3) read
/// per lane at `lane * 4` as a dword, its byte 1, and two dwords 4 bytes
/// further through `soffset`; a structured one (`OOB_SELECT` 1, stride 8)
/// read by `idxen` at the lane's element plus 4. Lane `l` stores the five
/// dwords at `32 * l` of the UAV at heap index 0.
fn code_table_program() -> Vec<u32> {
    let mut w = vec![s_mov_b32(5, ZERO as u8)];
    w.extend(s_load(2, 8, 4, 0)); // s[8:11]: the result UAV
    // s[12:15] and s[16:19]: the table's descriptors (the literals patched
    // below with the table's distance from each s_getpc_b64's next word).
    let raw = w.len();
    w.extend([0xBE8C_1F00, 0x800C_FF0C, 0, 0x820D_800D]); // s_getpc_b64 s[12:13]; s_add_u32; s_addc_u32
    w.extend([0xBE8E_03FF, RAW_RECORDS, 0xBE8F_03FF, 0x3101_4FAC]); // s_mov_b32 s14, s15
    let indexed = w.len();
    w.extend([0xBE90_1F00, 0x8010_FF10, 0, 0x8211_8011]); // s_getpc_b64 s[16:17]; s_add_u32; s_addc_u32
    w.extend([0x8811_FF11, 8 << 16]); // s_or_b32 s17, s17, stride 8
    w.extend([0xBE92_03FF, INDEXED_RECORDS, 0xBE93_03FF, 0x1101_4FAC]); // s_mov_b32 s18, s19
    w.push(0xBE94_0384); // s_mov_b32 s20, 4
    w.push(0x3402_0082); // v_lshlrev_b32 v1, 2, v0
    w.push(0x3402_0085 | 7 << 17); // v_lshlrev_b32 v7, 5, v0
    w.extend(mubuf(0x0C, 2, 1, 12, 0x80, OFFEN));
    w.extend(mubuf(0x08, 3, 1, 12, 0x80, OFFEN | 1));
    w.extend(mubuf(0x0C, 4, 0, 16, 0x80, IDXEN | 4));
    w.extend(mubuf(0x0D, 5, 1, 12, 20, OFFEN));
    w.push(S_WAITCNT_ALL);
    for k in 0..5u8 {
        w.extend(mubuf(0x1C, 2 + k, 7, 8, 0x80, OFFEN | (4 * u32::from(k))));
    }
    w.push(S_ENDPGM);
    let table = 4 * w.len() as u32;
    for at in [raw, indexed] {
        w[at + 2] = table - 4 * (at as u32 + 1);
    }
    w.extend(code_table());
    w
}

/// What lane `l` of [`code_table_program`] stores: out of range, a load
/// reads 0 (a raw load by dword, each checked as `offset + soffset +
/// size > records`; an indexed one by `index >= records`).
fn code_table_model(l: u32) -> [u32; 5] {
    let t = code_table();
    let raw = |byte: u32, size: u32, x: u32| if byte + size > RAW_RECORDS { 0 } else { x };
    [
        raw(4 * l, 4, t[l as usize]),
        raw(4 * l + 1, 1, t[l as usize] >> 8 & 0xFF),
        if l >= INDEXED_RECORDS {
            0
        } else {
            t[2 * l as usize + 1]
        },
        raw(4 * l + 4, 4, t[l as usize + 1]),
        raw(4 * l + 8, 4, t[l as usize + 2]),
    ]
}

fn lower_code_table_program(words: &[u32]) -> Result<HostShader, String> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 4,
            slot: 1,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let e = |e: crate::ShaderError| e.to_string();
    let p = ir::build(&native.code, native.wave64, native.vgprs).map_err(e)?;
    let st = structure::structure(&p).map_err(e)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).map_err(e)?;
    let io = io::raise(&p, &native).map_err(e)?;
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .map_err(e)?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        samplers: l.samplers,
        static_samplers: Vec::new(),
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    })
}

/// [`code_table_program`] decodes as written and lowers.
#[test]
fn code_table_buffer_loads_lower() {
    let words = code_table_program();
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let text: Vec<String> = crate::gfx10::decode(&code)
        .expect("the program decodes")
        .iter()
        .map(|i| i.text(false))
        .collect();
    let table = 4 * (words.len() - 64);
    let want = [
        "s_getpc_b64 s[12:13]".to_owned(),
        format!("s_add_u32 s12, s12, lit({:#010x})", table - 16),
        "s_addc_u32 s13, s13, 0".to_owned(),
        format!("s_mov_b32 s14, lit({RAW_RECORDS:#010x})"),
        "s_mov_b32 s15, lit(0x31014fac)".to_owned(),
        "s_getpc_b64 s[16:17]".to_owned(),
        format!("s_add_u32 s16, s16, lit({:#010x})", table - 48),
        "s_addc_u32 s17, s17, 0".to_owned(),
        "s_or_b32 s17, s17, lit(0x00080000)".to_owned(),
        format!("s_mov_b32 s18, lit({INDEXED_RECORDS:#010x})"),
        "s_mov_b32 s19, lit(0x11014fac)".to_owned(),
        "s_mov_b32 s20, 4".to_owned(),
        "v_lshlrev_b32 v1, 2, v0".to_owned(),
        "v_lshlrev_b32 v7, 5, v0".to_owned(),
        "buffer_load_dword v2, v1, s[12:15], 0 offen".to_owned(),
        "buffer_load_ubyte v3, v1, s[12:15], 0 offen offset:1".to_owned(),
        "buffer_load_dword v4, v0, s[16:19], 0 idxen offset:4".to_owned(),
        "buffer_load_dwordx2 v[5:6], v1, s[12:15], s20 offen".to_owned(),
    ];
    assert_eq!(text[2..2 + want.len()], want[..]);
    let shader = lower_code_table_program(&words).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    }
    // OOB_SELECT 0 (offsets checked against the stride) is refused.
    let mut checked = words.clone();
    let at = words
        .iter()
        .position(|&w| w == 0x3101_4FAC)
        .expect("dword 3");
    checked[at] = 0x0101_4FAC;
    let Err(e) = lower_code_table_program(&checked) else {
        panic!("OOB_SELECT 0 lowered");
    };
    assert!(e.contains("checking offsets against its stride"), "{e}");
}

/// [`code_table_program`] on the host GPU and WARP matches
/// [`code_table_model`] in every lane.
#[test]
fn code_table_buffer_loads_read_zeros_out_of_range() {
    let shader = lower_code_table_program(&code_table_program()).expect("lower");
    let want: Vec<u32> = (0..32)
        .flat_map(|l| {
            let mut row = code_table_model(l).to_vec();
            row.resize(8, 0);
            row
        })
        .collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            assert_eq!((u.root, u.slot), (RootType::Table, 1), "user data {u:?}");
            ud[u.index as usize] = 0;
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(32 * 8)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(out[0], want, "{label}");
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}
