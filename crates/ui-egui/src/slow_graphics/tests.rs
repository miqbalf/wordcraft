use egui::{Context, RawInput};

use super::{SLOW_GRAPHICS, dismiss, is_software, renderer, show, showing};

fn frame(ctx: &Context) -> usize {
    let out = ctx.run_ui(RawInput::default(), |ui| show(ui.ctx()));
    let shapes = out.shapes.len();
    out.drop_without_applying_deltas();
    shapes
}

#[test]
fn software_renderers_are_told_apart_from_gpus() {
    for name in [
        "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero)), SwiftShader driver)",
        "llvmpipe (LLVM 17.0.6, 256 bits)",
        "Microsoft Basic Render Driver Software Adapter",
        "Google SwiftShader",
        "ANGLE (Microsoft, Microsoft Basic Render Driver (0x0000008C) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ] {
        assert!(is_software(name), "{name}");
    }
    for name in [
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 (0x00002503) Direct3D11 vs_5_0 ps_5_0, D3D11)",
        "Apple M2",
        "Mesa Intel(R) UHD Graphics 620 (KBL GT2)",
        "",
    ] {
        assert!(!is_software(name), "{name}");
    }
}

#[test]
fn a_software_renderer_shows_the_notice_once_until_dismissed() {
    let ctx = Context::default();
    assert!(!showing(&ctx), "nothing before the renderer is known");
    let empty = frame(&ctx);
    renderer(&ctx, "llvmpipe (LLVM 17.0.6, 256 bits)", false);
    assert!(showing(&ctx));
    assert!(frame(&ctx) > empty, "the notice is drawn");
    dismiss(&ctx);
    assert!(!showing(&ctx));
    renderer(&ctx, "llvmpipe (LLVM 17.0.6, 256 bits)", false);
    assert!(!showing(&ctx), "once dismissed it does not come back");
    assert_eq!(frame(&ctx), empty);
    assert!(SLOW_GRAPHICS.ends_with("slow."));
}

#[test]
fn a_gpu_shows_nothing_even_if_a_later_report_says_software() {
    let ctx = Context::default();
    renderer(&ctx, "Apple M2", false);
    renderer(&ctx, "SwiftShader", false);
    assert!(!showing(&ctx), "only the first report counts");
    let ctx = Context::default();
    renderer(&ctx, "", true);
    assert!(showing(&ctx), "a CPU device by the API's own word");
}
