# Hardware

Scarlight is built for Mesa's open Vulkan drivers, not for one card. This page records what has
actually been run, what we expect, and what nobody has looked at yet.

## Status

| **Tested** | **Expected** | **Unknown** |
|---|---|---|
| **RX 9070 XT (RDNA4) · RADV**<br>The only hardware this has been run on: every published number comes from it, Mesa 26.3.1 | **RDNA3, RDNA2, RDNA1 (RX 7000 / 6000 / 5000) and Vega · RADV**<br>Same driver, older generations. Not yet checked.<br><br>**Arc A-series, Arc B-series, Xe2 · ANV**<br>A second open driver. Not yet checked. | **Intel Gen9 and older integrated · ANV**<br>**Polaris and older (GCN 4 and earlier) · RADV**<br>**AMD APUs**<br>Any Mesa older than the one we test<br>Not yet checked. |

**Tested** means a full run on the fixed test scene, with the picture looked at, on a named Mesa
version. **Expected** means the card has a Mesa driver this project is aimed at and no known blocker.
**Unknown** covers everything else: it may work, and nobody has said. The last two columns move to
Tested only with a report, so treat "Expected" as "we think so", never as a promise.

NVIDIA's NVK driver is a roadmap item, not an expected target yet: see [ROADMAP.md](ROADMAP.md).

Native Windows and NVIDIA's proprietary driver are not supported. The build is a Linux project: it
runs through Wine and vkd3d-proton on Mesa, and no path here exists for a closed driver.

## What we need from a report

A report only needs numbers and text: no screenshots, game files or hashes of game data
([asset policy](asset-policy.md)). The list of what to send is in the README under
[help us test](../README.md#help-us-test), and a form asks for each item:

- [test report](https://github.com/pablogonz12/Scarlight/issues/new?template=test-report.yml) for a
  run, good or bad;
- [bug report](https://github.com/pablogonz12/Scarlight/issues/new?template=bug-report.yml) for a
  crash, a hang or a wrong picture.

A row moves when a report shows the build reaching the world and the picture being right enough to
look at. A card that fails is at least as useful: a failure on a gated path is our bug, and a failure
in the driver goes to Mesa with the report attached.

## Known driver-side questions

These are open questions, not conclusions. Each one is written down so a report can settle it:

- **Mesa minimum.** We test on one Mesa version. The oldest version that works is to be confirmed.
- **Kernel minimum.** `amdgpu`, `i915` and `xe` all change; the oldest kernel that works is to be
  confirmed.
- **Vulkan 1.3 features.** vkd3d-proton needs Vulkan 1.3. Which optional features a driver must also
  expose (`VK_EXT_robustness2`, descriptor buffers, host memory import) is still being mapped out.
- **Wave width.** Some console programs assume 64-lane waves. On a 32-lane host they are emulated;
  which Mesa GPUs hit that path most is unmeasured.
