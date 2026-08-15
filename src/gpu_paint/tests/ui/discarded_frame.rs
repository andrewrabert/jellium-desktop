#![deny(warnings)]

fn discards_the_frame(swapchain: &mut jfn_gpu_paint::Swapchain<'_>) {
    swapchain.acquire();
}

fn main() {}
