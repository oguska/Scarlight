//! End-to-end D3D12 tests across every SM 6.6 hardware adapter and WARP
//! (`dxil.dll` validated) for buffer descriptor stride bitfields, merged
//! multi-descriptor loads, 64-bit half-dead register moves/selects, and
//! safety refusals on descriptor base-address reads.

use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, StaticSampler, lower, validator};
use crate::ShaderError;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

const fn sgpr(n: u8) -> u16 {
    n as u16
}

const fn vgpr(n: u8) -> u16 {
    256 + n as u16
}

const fn inline_u32(n: u8) -> u16 {
    128 + n as u16
}

const fn sop1(op: u8, sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32 & 0x7F) << 16 | (op as u32) << 8 | src0 as u32
}

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    sop1(0x03, sdst, src0)
}

const fn s_mov_b64(sdst: u8, src0: u8) -> u32 {
    sop1(0x04, sdst, src0)
}

const fn sop2(op: u8, sdst: u8, src0: u8, src1: u8) -> u32 {
    0x8000_0000
        | (op as u32 & 0x7F) << 23
        | (sdst as u32 & 0x7F) << 16
        | (src1 as u32) << 8
        | src0 as u32
}

const fn sopc(op: u8, src0: u8, src1: u8) -> u32 {
    0xBF00_0000 | (op as u32 & 0x7F) << 16 | (src1 as u32) << 8 | src0 as u32
}

const fn smem_load(op: u8, dst: u8, base: u8, byte_off: u32) -> [u32; 2] {
    [
        0x3D << 26 | (op as u32) << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        (125 << 25) | (byte_off & 0x1F_FFFF),
    ]
}

const fn s_load_dwordx4(dst: u8, base: u8, byte_off: u32) -> [u32; 2] {
    smem_load(0x02, dst, base, byte_off)
}

const fn s_load_dwordx16(dst: u8, base: u8, byte_off: u32) -> [u32; 2] {
    smem_load(0x04, dst, base, byte_off)
}

const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32 & 0x3F) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn vop3(op: u16, vdst: u8, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
    [
        0x35 << 26 | (op as u32 & 0x3FF) << 16 | vdst as u32,
        (src0 as u32 & 0x1FF) | (src1 as u32 & 0x1FF) << 9 | (src2 as u32 & 0x1FF) << 18,
    ]
}

const fn mubuf(
    op: u8,
    vdata: u8,
    vaddr: u8,
    srsrc: u8,
    offen: bool,
    idxen: bool,
    byte_off: u16,
) -> [u32; 2] {
    [
        0x38 << 26
            | (op as u32 & 0x7F) << 18
            | (idxen as u32) << 13
            | (offen as u32) << 12
            | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

const fn mimg_sample_lz(
    vdata: u8,
    vaddr: u8,
    srsrc: u8,
    ssamp: u8,
    dmask: u8,
    dim: u8,
) -> [u32; 2] {
    [
        0xF09C_0000 | (dmask as u32 & 0xF) << 8 | (dim as u32 & 7) << 3,
        ((ssamp as u32 >> 2) & 0x1F) << 21
            | ((srsrc as u32 >> 2) & 0x1F) << 16
            | (vdata as u32) << 8
            | vaddr as u32,
    ]
}

fn translate_cs(
    words: &[u32],
    threads: u32,
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
) -> Result<HostShader, ShaderError> {
    let sgprs: u32 = layout.iter().map(|p| p.sgprs).sum();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: sgprs.max(2) << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, false, 32)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    let lowered = lower::lower(&lower::Parts {
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
    })?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        static_samplers: lowered
            .samplers
            .iter()
            .map(|&w| StaticSampler::from_words(w))
            .collect(),
        samplers: lowered.samplers,
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        memory: lowered.memory,
        memory_writes: lowered.memory_writes,
        view_addresses: lowered.view_addresses,
        heap: lowered.heap,
        heap_writes: lowered.heap_writes,
        view_marks: lowered.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    Ok(shader)
}

/// Every SM 6.6 hardware GPU adapter (non-software with dedicated video memory,
/// matching `Host::new(false)` on headless CI) plus WARP (`Host::new(true)`).
fn sm66_hosts() -> Vec<(String, Host)> {
    let mut hosts = Vec::new();
    // SAFETY: plain DXGI/D3D12 adapter enumeration and feature checks.
    unsafe {
        if let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) {
            for idx in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(idx) else {
                    break;
                };
                let Ok(desc) = adapter.GetDesc1() else {
                    continue;
                };
                let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
                if software || desc.DedicatedVideoMemory == 0 {
                    continue;
                }
                let mut dev: Option<ID3D12Device> = None;
                if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut dev).is_err() {
                    continue;
                }
                let Some(device) = dev else { continue };
                let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL {
                    HighestShaderModel: D3D_SHADER_MODEL_6_6,
                };
                if device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_SHADER_MODEL,
                        (&raw mut sm).cast(),
                        size_of_val(&sm) as u32,
                    )
                    .is_ok()
                    && sm.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0
                {
                    let len = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    let name = String::from_utf16_lossy(&desc.Description[..len]);
                    hosts.push((name, Host { device }));
                }
            }
        }
    }
    if hosts.is_empty() {
        eprintln!("no SM 6.6 hardware device; skipped");
    }
    if let Ok(warp) = Host::new(true) {
        hosts.push(("WARP".into(), warp));
    } else {
        eprintln!("no SM 6.6 WARP device; skipped");
    }
    hosts
}

fn run_cs(shader: &HostShader, constants: &[u32], views: &[View]) -> Vec<(String, Vec<u32>)> {
    let mut out = Vec::new();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, host) in sm66_hosts() {
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if u.root == RootType::Constants {
                ud[u.index as usize] = constants.get(u.dword as usize).copied().unwrap_or(0);
            } else {
                ud[u.index as usize] = 0;
            }
        }
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words[0].clone()));
    }
    assert!(
        !out.is_empty(),
        "neither hardware nor WARP SM 6.6 device available"
    );
    out
}

fn root_param(root: RootType, sgprs: u32, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    }
}

fn used_range(
    kind: ResourceType,
    table_offset_dwords: u32,
    dwords: u8,
    stride: u32,
    user_data: u8,
    slot: u8,
) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords,
        user_data,
        slot,
    }
}

#[test]
fn buffer_stride_bitfields_merged_loads_and_half_dead_b64_moves_run_on_all_gpus() {
    let mut w = vec![s_mov_b32(4, 1), s_mov_b32(5, 0x80)];
    w.extend_from_slice(&s_load_dwordx16(8, 4, 0x0)); // slots 0 and 1 together
    w.extend_from_slice(&s_load_dwordx4(20, 4, 0x40)); // UAV at slot 2
    w.push(vop2(0x1A, 16, inline_u32(2), 0)); // v16 = v0 << 2
    w.push(vop2(0x1B, 2, inline_u32(3), 0)); // v2 = v0 & 3

    // 1. `s_bfe_u32 s24, s9, (14 << 16) | 16` -> stride0 = 12
    w.push(sop2(0x27, 24, 9, 0xFF));
    w.push((14 << 16) | 16);
    // 2. `v_bfe_u32 v4, v3, 16, 14` and `(v3 << 2) >> 18` -> 12
    w.push(vop1(0x01, 3, sgpr(9)));
    w.extend_from_slice(&vop3(0x148, 4, vgpr(3), inline_u32(16), inline_u32(14)));
    w.push(vop2(0x1A, 5, inline_u32(2), 3));
    w.push(vop2(0x16, 5, inline_u32(18), 5));
    // 3. `(s17 >> 16) & 0x3fff` -> stride1 = 16
    w.push(sop2(0x20, 25, 17, 128 + 16));
    w.push(sop2(0x0E, 25, 25, 0xFF));
    w.push(0x3FFF);
    // 4. Half-dead 64-bit move & select: `s[26:27] = s[10:11]`,
    //    `s[30:31] = (s0 == 0) ? s[18:19] : s[26:27]`
    w.push(s_mov_b64(26, 10));
    w.push(sopc(0x06, 0, 0x80)); // s_cmp_eq_u32 s0, 0
    w.push(sop2(0x0B, 30, 18, 26)); // s_cselect_b64 s[30:31], s[18:19], s[26:27]

    // Compute byte offset into slot 0 using extracted `s24` (12): `v6 = v2 * s24`
    w.extend_from_slice(&vop3(0x169, 6, vgpr(2), sgpr(24), 0));
    w.extend_from_slice(&mubuf(0x0C, 1, 6, 8, true, false, 4)); // 200 + (v0 & 3)
    w.extend_from_slice(&mubuf(0x0C, 7, 2, 16, false, true, 8)); // 600 + (v0 & 3)

    // Sum: v1 + v7 + v4 (12) + v5 (12) + s24 (12) + s25 (16) + s26 (6) + s30 (5 or 6)
    w.extend_from_slice(&vop3(0x125, 1, vgpr(7), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, vgpr(4), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, vgpr(5), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, sgpr(24), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, sgpr(25), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, sgpr(26), vgpr(1), 0));
    w.extend_from_slice(&vop3(0x125, 1, sgpr(30), vgpr(1), 0));
    w.extend_from_slice(&mubuf(0x1C, 1, 16, 20, true, false, 0));
    w.push(S_ENDPGM);

    let layout = vec![
        root_param(RootType::Constants, 1, 0, 0),
        root_param(RootType::Table, 1, 1, 1),
    ];
    let used = vec![
        used_range(ResourceType::Srv, 0, 4, 12, 1, 1),
        used_range(ResourceType::Srv, 8, 4, 16, 1, 1),
        used_range(ResourceType::Uav, 16, 4, 0, 1, 1),
    ];
    let shader = translate_cs(&w, 32, layout, used).expect("translates");
    let mut buf0 = Vec::new();
    for r in 0..6u32 {
        buf0.extend_from_slice(&[100 + r, 200 + r, 300 + r]);
    }
    let mut buf1 = Vec::new();
    for r in 0..5u32 {
        buf1.extend_from_slice(&[400 + r, 500 + r, 600 + r, 700 + r]);
    }
    let views = [
        View::Buffer {
            data: &buf0,
            stride: Stride::Raw,
        },
        View::Buffer {
            data: &buf1,
            stride: Stride::Structured(16),
        },
        View::RawUav(32),
    ];
    for (mode, picked) in [(0u32, 5u32), (1, 6)] {
        let runs = run_cs(&shader, &[mode], &views);
        for (label, got) in &runs {
            eprintln!("verified on adapter: {label}");
            let want: Vec<u32> = (0..32)
                .map(|l| (200 + (l & 3)) + (600 + (l & 3)) + 12 + 12 + 12 + 16 + 6 + picked)
                .collect();
            assert_eq!(*got, want, "{label} (mode {mode})");
        }
    }
}

#[test]
fn descriptor_address_and_unknown_stride_reads_are_refused() {
    let make = |extract: &[u32]| -> Vec<u32> {
        let mut w = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
        w.extend_from_slice(&s_load_dwordx4(8, 4, 0x0));
        w.extend_from_slice(&s_load_dwordx4(16, 4, 0x20));
        w.extend_from_slice(&mubuf(0x0C, 1, 0, 8, false, true, 0));
        w.extend_from_slice(extract);
        w.extend_from_slice(&vop3(0x125, 1, sgpr(24), vgpr(1), 0));
        w.extend_from_slice(&mubuf(0x1C, 1, 0, 16, false, false, 0));
        w.push(S_ENDPGM);
        w
    };
    let layout = || vec![root_param(RootType::Table, 1, 0, 0)];
    let used_with_stride = |stride: u32| {
        vec![
            used_range(ResourceType::Srv, 0, 4, stride, 0, 0),
            used_range(ResourceType::Uav, 8, 4, 0, 0, 0),
        ]
    };

    let cases: &[(&str, &[u32], u32)] = &[
        ("dword 0 base address", &[s_mov_b32(24, 8)], 16),
        (
            "dword 1 low 16 address bits",
            &[sop2(0x27, 24, 9, 0xFF), 16 << 16],
            16,
        ),
        (
            "dword 1 overlapping bit 15",
            &[sop2(0x27, 24, 9, 0xFF), (14 << 16) | 15],
            16,
        ),
        ("dword 1 unmasked >> 16", &[sop2(0x20, 24, 9, 128 + 16)], 16),
        (
            "dword 1 stride bits with unknown stride == 0",
            &[sop2(0x27, 24, 9, 0xFF), (14 << 16) | 16],
            0,
        ),
    ];
    for &(case, extract, stride) in cases {
        let code = make(extract);
        let err = translate_cs(&code, 1, layout(), used_with_stride(stride)).unwrap_err();
        assert!(
            matches!(
                err,
                ShaderError::Unsupported {
                    what: "a descriptor's address or format read as data",
                    ..
                }
            ),
            "{case}: {err:?}"
        );
    }
}

#[test]
fn merged_reload_slices_supply_all_record_counts_and_strides_on_all_gpus() {
    // Covers both:
    // 1. Direct slice 0 on load A + reload B supplying slice 8 (`direct_first = true`);
    // 2. Pure scalar-read load 0 + reload A supplying slice 0 + reload B supplying slice 8,
    //    either via a full `s_load_dwordx16` at 0x0 or a narrow `s_load_dwordx4` at 0x20.
    let build_case = |direct_first: bool, narrow_second_reload: bool| -> Vec<u32> {
        let mut w = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
        w.extend_from_slice(&s_load_dwordx16(8, 4, 0x0)); // s[8:23]: scalar metadata read from here
        w.extend_from_slice(&s_load_dwordx4(56, 4, 0x40)); // UAV at slot 2
        if !direct_first {
            w.extend_from_slice(&s_load_dwordx16(24, 4, 0x0)); // reload A supplies slice 0
        }
        if narrow_second_reload {
            w.extend_from_slice(&s_load_dwordx4(48, 4, 0x20)); // reload B at +0x20 supplies slice 8
        } else {
            w.extend_from_slice(&s_load_dwordx16(40, 4, 0x0)); // reload B at 0x0 supplies slice 8
        }
        let rsrc0 = if direct_first { 8 } else { 24 };
        let rsrc1 = 48;
        w.push(vop2(0x1A, 16, inline_u32(2), 0)); // v16 = v0 << 2
        w.push(vop2(0x1B, 2, inline_u32(3), 0)); // v2 = v0 & 3

        // Read both slices' stride bits and record counts from the initial `s[8:23]` load!
        w.push(sop2(0x27, 60, 9, 0xFF)); // s60 = stride0 (12)
        w.push((14 << 16) | 16);
        w.push(sop2(0x27, 61, 17, 0xFF)); // s61 = stride1 (16)
        w.push((14 << 16) | 16);

        w.extend_from_slice(&vop3(0x169, 6, vgpr(2), sgpr(60), 0)); // v6 = v2 * 12
        w.extend_from_slice(&mubuf(0x0C, 1, 6, rsrc0, true, false, 4)); // 200 + (v0 & 3)
        w.extend_from_slice(&mubuf(0x0C, 7, 2, rsrc1, false, true, 8)); // 600 + (v0 & 3)

        // Sum: v1 + v7 + s60 (12) + s10 (count0 = 6) + s61 (16) + s18 (count1 = 5)
        w.extend_from_slice(&vop3(0x125, 1, vgpr(7), vgpr(1), 0));
        w.extend_from_slice(&vop3(0x125, 1, sgpr(60), vgpr(1), 0));
        w.extend_from_slice(&vop3(0x125, 1, sgpr(10), vgpr(1), 0));
        w.extend_from_slice(&vop3(0x125, 1, sgpr(61), vgpr(1), 0));
        w.extend_from_slice(&vop3(0x125, 1, sgpr(18), vgpr(1), 0));
        w.extend_from_slice(&mubuf(0x1C, 1, 16, 56, true, false, 0));
        w.push(S_ENDPGM);
        w
    };

    let mut buf0 = Vec::new();
    for r in 0..6u32 {
        buf0.extend_from_slice(&[100 + r, 200 + r, 300 + r]);
    }
    let mut buf1 = Vec::new();
    for r in 0..5u32 {
        buf1.extend_from_slice(&[400 + r, 500 + r, 600 + r, 700 + r]);
    }
    let views = [
        View::Buffer {
            data: &buf0,
            stride: Stride::Raw,
        },
        View::Buffer {
            data: &buf1,
            stride: Stride::Structured(16),
        },
        View::RawUav(32),
    ];
    let want: Vec<u32> = (0..32)
        .map(|l| (200 + (l & 3)) + (600 + (l & 3)) + 12 + 6 + 16 + 5)
        .collect();

    for (direct_first, narrow_second) in [(true, false), (false, false), (false, true)] {
        let layout = vec![root_param(RootType::Table, 1, 0, 0)];
        let used = vec![
            used_range(ResourceType::Srv, 0, 4, 12, 0, 0),
            used_range(ResourceType::Srv, 8, 4, 16, 0, 0),
            used_range(ResourceType::Uav, 16, 4, 0, 0, 0),
        ];
        let code = build_case(direct_first, narrow_second);
        let shader = translate_cs(&code, 32, layout, used).expect("translates");
        for (label, got) in run_cs(&shader, &[], &views) {
            assert_eq!(
                got, want,
                "{label} (direct {direct_first}, narrow {narrow_second})"
            );
        }
    }
}

#[test]
fn merged_cube_and_2d_slices_sample_and_read_dimensions_on_all_gpus() {
    let mut w = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
    // Merged 16-dword load: slot 0 (`s[8:15]`) is a 4x4 Cube, slot 1 (`s[16:23]`) is a 6x3 2D SRV.
    w.extend_from_slice(&s_load_dwordx16(8, 4, 0x0));
    w.extend_from_slice(&s_load_dwordx4(24, 4, 0x40)); // UAV at slot 2
    // Immediate point-wrap sampler in `s[28:31] = [0, 0, 0, 0]`.
    for s in 28..=31 {
        w.push(s_mov_b32(s, 0x80));
    }
    w.push(vop2(0x1A, 16, inline_u32(2), 0)); // v16 = v0 << 2

    // Cube sample coordinates in `v[3:5]`: `s = 1.5`, `t = 1.5`, `face = float(v0 % 6)`, `lod = 0.0` in `v6`.
    w.extend_from_slice(&[vop1(0x01, 3, 0xFF), 0x3FC0_0000]); // v3 = 1.5
    w.extend_from_slice(&[vop1(0x01, 4, 0xFF), 0x3FC0_0000]); // v4 = 1.5
    // `v5 = v0 & 3` (faces 0..=3: +X, -X, +Y, -Y), converted to f32 via `v_cvt_f32_u32` (VOP1 0x06).
    w.push(vop2(0x1B, 5, inline_u32(3), 0));
    w.push(vop1(0x06, 5, vgpr(5)));
    w.push(vop1(0x01, 6, inline_u32(0))); // v6 = 0.0 (unused by `_lz`, clean reg)
    w.extend_from_slice(&mimg_sample_lz(2, 3, 8, 28, 0x1, 3)); // v2 = cube sample (dim = 3 CUBE)

    // 2D sample coordinates in `v[7:8] = (0.5, 0.5)`: `image_sample_lz` -> `v9`.
    w.push(vop1(0x01, 7, 0xF0)); // 0.5f32 inline constant
    w.push(vop1(0x01, 8, 0xF0)); // 0.5f32 inline constant
    w.extend_from_slice(&mimg_sample_lz(9, 7, 16, 28, 0x1, 1)); // v9 = 2D sample (dim = 1 2D)

    // Convert sampled floats `v2` and `v9` to u32 (`v_cvt_u32_f32` = VOP1 0x07) and sum with
    // reconstructed scalar metadata from BOTH slices:
    // - 2D slice (`s[16:23]`): `width - 1` = `(s17 >> 30) | ((s18 & 0x3fff) << 2)` = 5,
    //   `height - 1` = `(s18 >> 14) & 0x3fff` = 2;
    // - Cube slice (`s[8:15]`): `face_width - 1` = `(s9 >> 30) | ((s10 & 0x3fff) << 2)` = 3,
    //   `type` = `s11 >> 28` = 11 (`SQ_RSRC_IMG_CUBE`).
    w.push(vop1(0x07, 2, vgpr(2)));
    w.push(vop1(0x07, 9, vgpr(9)));
    w.push(sop2(0x20, 32, 17, 128 + 30)); // s32 = s17 >> 30
    w.extend_from_slice(&[sop2(0x0E, 33, 18, 0xFF), 0x3FFF]); // s33 = s18 & 0x3fff
    w.push(sop2(0x1E, 33, 33, 128 + 2)); // s33 <<= 2
    w.push(sop2(0x10, 32, 32, 33)); // s32 = 2D width - 1 = 5
    w.extend_from_slice(&[sop2(0x27, 34, 18, 0xFF), (14 << 16) | 14]); // s34 = 2D height - 1 = 2
    w.push(sop2(0x20, 35, 9, 128 + 30));
    w.extend_from_slice(&[sop2(0x0E, 36, 10, 0xFF), 0x3FFF]);
    w.push(sop2(0x1E, 36, 36, 128 + 2));
    w.push(sop2(0x10, 35, 35, 36)); // s35 = cube face_width - 1 = 3
    w.push(sop2(0x20, 37, 11, 128 + 28)); // s37 = cube TYPE = 11

    w.extend_from_slice(&vop3(0x125, 2, vgpr(9), vgpr(2), 0));
    for s in [32, 34, 35, 37] {
        w.extend_from_slice(&vop3(0x125, 2, sgpr(s), vgpr(2), 0));
    }
    w.extend_from_slice(&mubuf(0x1C, 2, 16, 24, true, false, 0));
    w.push(S_ENDPGM);

    let layout = vec![root_param(RootType::Table, 1, 0, 0)];
    let used = vec![
        used_range(ResourceType::Srv, 0, 8, 0, 0, 0),
        used_range(ResourceType::Srv, 8, 8, 0, 0, 0),
        used_range(ResourceType::Uav, 16, 4, 0, 0, 0),
    ];
    let shader = translate_cs(&w, 32, layout, used).expect("cube + 2D merged load translates");

    let mut cube_texels = Vec::with_capacity(6 * 16);
    for face in 0..6u32 {
        for _ in 0..16 {
            cube_texels.push([(10 * (face + 1)) as f32, 0.0, 0.0, 1.0]);
        }
    }
    let tex2d = vec![[7.0f32, 0.0, 0.0, 1.0]; 6 * 3];
    let views = [
        View::Cube(4, &cube_texels),
        View::Texture(6, 3, &tex2d),
        View::RawUav(32),
    ];
    let want: Vec<u32> = (0..32)
        .map(|l| 10 * ((l & 3) + 1) + 7 + 5 + 2 + 3 + 11)
        .collect();
    for (label, got) in run_cs(&shader, &[], &views) {
        assert_eq!(got, want, "{label}");
    }

    // Per-slice refusal isolation: an unreconstructable CBV slice at `s[8:11]`
    // in a merged `s_load_dwordx16` does not block scalar `NUM_RECORDS` reads
    // of a sibling structured SRV slice at `s[16:19]`, while reading `s10` of
    // the CBV slice itself still reports `"a constant buffer's descriptor read as data"`.
    let make_cbv_srv = |read_cbv_size: bool| -> Vec<u32> {
        let mut c = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
        c.extend_from_slice(&s_load_dwordx16(8, 4, 0x0));
        c.extend_from_slice(&s_load_dwordx4(24, 4, 0x40));
        c.extend_from_slice(&mubuf(0x0C, 1, 0, 8, false, false, 0)); // CBV load from slice 0
        c.extend_from_slice(&mubuf(0x0C, 2, 0, 16, false, true, 0)); // SRV load from slice 8
        let size_reg = if read_cbv_size { 10 } else { 18 };
        c.extend_from_slice(&vop3(0x125, 1, vgpr(2), vgpr(1), 0));
        c.extend_from_slice(&vop3(0x125, 1, sgpr(size_reg), vgpr(1), 0));
        c.extend_from_slice(&mubuf(0x1C, 1, 0, 24, false, false, 0));
        c.push(S_ENDPGM);
        c
    };
    let cbv_srv_used = || {
        vec![
            used_range(ResourceType::Cbv, 0, 4, 0, 0, 0),
            used_range(ResourceType::Srv, 8, 4, 4, 0, 0),
            used_range(ResourceType::Uav, 16, 4, 0, 0, 0),
        ]
    };
    translate_cs(
        &make_cbv_srv(false),
        1,
        vec![root_param(RootType::Table, 1, 0, 0)],
        cbv_srv_used(),
    )
    .expect("SRV slice metadata lowers beside unread CBV slice");
    let cbv_err = translate_cs(
        &make_cbv_srv(true),
        1,
        vec![root_param(RootType::Table, 1, 0, 0)],
        cbv_srv_used(),
    )
    .unwrap_err();
    assert!(
        matches!(
            cbv_err,
            ShaderError::Unsupported {
                what: "a constant buffer's descriptor read as data",
                ..
            }
        ),
        "{cbv_err:?}"
    );
}
