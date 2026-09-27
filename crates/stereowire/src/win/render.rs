//! Direct3D 11 rendering of decoded frames.
//!
//! A `DXGI_FORMAT_NV12` texture is never created: doing so crashes
//! CrossOver's D3D11-on-Metal translation with an uncatchable Metal
//! assertion. Luma and chroma are uploaded as separate R8_UNORM and
//! R8G8_UNORM textures instead, each a DEFAULT texture fed from a STAGING
//! one, which is the path that survives there.

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use super::decoder::DecodedFrame;

/// Fullscreen-triangle vertex shader plus a BT.709 pixel shader. The decoder
/// already crops to the display area, so the textures are always frame
/// sized and sampling needs no UV scaling constant. It also already
/// normalizes luma to full range no matter what the decoder itself
/// delivered (see the module doc comment on `win::decoder`), which the
/// self-test's byte comparison needs to mean the same thing on both
/// platforms, so only chroma still needs the limited-range Cb/Cr to
/// signed-deviation step here.
const SHADER_SOURCE: &str = r#"
struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VSOut VSMain(uint id : SV_VertexID) {
    // Big-triangle trick: three vertices that cover the whole viewport, no
    // vertex buffer needed. Clip-space corners: (-1,-1) (-1,3) (3,-1)
    float2 pos = float2((id == 2) ? 3.0 : -1.0, (id == 1) ? 3.0 : -1.0);
    VSOut o;
    o.pos = float4(pos, 0.0, 1.0);
    o.uv = float2((pos.x + 1.0) * 0.5, 1.0 - (pos.y + 1.0) * 0.5);
    return o;
}

Texture2D<float>  LumaTex   : register(t0);
Texture2D<float2> ChromaTex : register(t1);
SamplerState Samp : register(s0);

float4 PSMain(VSOut input) : SV_TARGET {
    float yp = LumaTex.Sample(Samp, input.uv);
    float2 c = ChromaTex.Sample(Samp, input.uv);

    // BT.709 limited range -> a signed deviation from centre, full range
    float cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);

    float r = yp + 1.5748 * cr;
    float g = yp - 0.1873 * cb - 0.4681 * cr;
    float b = yp + 1.8556 * cb;
    return float4(r, g, b, 1.0);
}
"#;

struct Textures {
    luma_default: ID3D11Texture2D,
    luma_staging: ID3D11Texture2D,
    luma_srv: ID3D11ShaderResourceView,
    chroma_default: ID3D11Texture2D,
    chroma_staging: ID3D11Texture2D,
    chroma_srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    client_width: u32,
    client_height: u32,
    textures: Option<Textures>,
}

impl Renderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let (device, context) = create_device()?;

        let mut rect = RECT::default();
        unsafe { GetClientRect(hwnd, &mut rect) }.context("GetClientRect failed")?;
        let client_width = (rect.right - rect.left).max(1) as u32;
        let client_height = (rect.bottom - rect.top).max(1) as u32;

        let swap_chain = create_swapchain(&device, hwnd, client_width, client_height)?;
        let rtv = create_rtv(&device, &swap_chain)?;
        let (vs, ps) = compile_shaders(&device)?;
        let sampler = create_sampler(&device)?;

        let renderer = Renderer {
            device,
            context,
            swap_chain,
            rtv: Some(rtv),
            vs,
            ps,
            sampler,
            client_width,
            client_height,
            textures: None,
        };
        renderer.clear();
        Ok(renderer)
    }

    /// Paints the client area black, so the window shows something settled
    /// while it waits for the first frame.
    fn clear(&self) {
        if let Some(rtv) = &self.rtv {
            unsafe {
                self.context
                    .ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
                let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
            }
        }
    }

    /// Resizes the swap chain to a new client area. A no-op if the size did
    /// not actually change.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || (width == self.client_width && height == self.client_height)
        {
            return Ok(());
        }
        // The render target view must be released before ResizeBuffers, or
        // the call fails with an outstanding reference to the back buffer.
        self.rtv = None;
        unsafe {
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .context("ResizeBuffers failed")?;
        self.rtv = Some(create_rtv(&self.device, &self.swap_chain)?);
        self.client_width = width;
        self.client_height = height;
        Ok(())
    }

    /// Uploads a decoded frame and presents it, aspect-fit and centred in
    /// the client area. Recreates the upload textures when the frame size
    /// changes.
    pub fn draw(&mut self, frame: &DecodedFrame) -> Result<()> {
        self.ensure_textures(frame.width as u32, frame.height as u32)?;
        self.upload(frame)?;
        self.render(frame.width as u32, frame.height as u32)
    }

    fn ensure_textures(&mut self, width: u32, height: u32) -> Result<()> {
        if self
            .textures
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height)
        {
            return Ok(());
        }
        self.textures = Some(create_textures(&self.device, width, height)?);
        Ok(())
    }

    fn upload(&self, frame: &DecodedFrame) -> Result<()> {
        let textures = self.textures.as_ref().context("textures not created")?;
        upload_planes(&self.context, textures, frame)
    }

    fn render(&self, frame_width: u32, frame_height: u32) -> Result<()> {
        let rtv = self.rtv.as_ref().context("render target missing")?;
        let textures = self.textures.as_ref().context("textures not created")?;
        unsafe {
            self.context
                .ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);

            let viewport = fit_viewport(
                self.client_width,
                self.client_height,
                frame_width,
                frame_height,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);

            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.PSSetShaderResources(
                0,
                Some(&[
                    Some(textures.luma_srv.clone()),
                    Some(textures.chroma_srv.clone()),
                ]),
            );

            self.context.Draw(3, 0);
            // Sync interval 0: we already pace ourselves by the incoming
            // stream, and a live view wants the newest frame, not to wait
            // for vblank.
            let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
        }
        Ok(())
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    for (driver_type, label) in [
        (D3D_DRIVER_TYPE_HARDWARE, "hardware"),
        (D3D_DRIVER_TYPE_WARP, "WARP"),
    ] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                HMODULE::default(),
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => {
                println!("renderer: D3D11 device created ({label})");
                return Ok((
                    device.context("D3D11CreateDevice produced no device")?,
                    context.context("D3D11CreateDevice produced no context")?,
                ));
            }
            Err(e) => println!("renderer: D3D11CreateDevice({label}) failed: {e}"),
        }
    }
    bail!("no D3D11 driver type succeeded (neither hardware nor WARP)")
}

fn create_swapchain(
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    let dxgi_device: IDXGIDevice = device.cast().context("device has no IDXGIDevice")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("GetAdapter failed")?;
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }.context("GetParent failed")?;

    let flip_desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    if let Ok(swap_chain) =
        unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &flip_desc, None, None) }
    {
        return Ok(swap_chain);
    }
    // FLIP_DISCARD is not universally available; DISCARD with one buffer is
    // the fallback that still works everywhere this app runs.
    let discard_desc = DXGI_SWAP_CHAIN_DESC1 {
        BufferCount: 1,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        ..flip_desc
    };
    unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &discard_desc, None, None) }
        .context("CreateSwapChainForHwnd failed")
}

fn create_rtv(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView> {
    let back_buffer: ID3D11Texture2D =
        unsafe { swap_chain.GetBuffer(0) }.context("GetBuffer failed")?;
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back_buffer, None, Some(&mut rtv)) }
        .context("CreateRenderTargetView failed")?;
    rtv.context("CreateRenderTargetView produced no view")
}

fn compile_shaders(device: &ID3D11Device) -> Result<(ID3D11VertexShader, ID3D11PixelShader)> {
    let vs_blob = compile_shader("VSMain", "vs_5_0")?;
    let ps_blob = compile_shader("PSMain", "ps_5_0")?;
    let vs_bytes = unsafe {
        std::slice::from_raw_parts(
            vs_blob.GetBufferPointer() as *const u8,
            vs_blob.GetBufferSize(),
        )
    };
    let ps_bytes = unsafe {
        std::slice::from_raw_parts(
            ps_blob.GetBufferPointer() as *const u8,
            ps_blob.GetBufferSize(),
        )
    };

    let mut vs = None;
    unsafe { device.CreateVertexShader(vs_bytes, None, Some(&mut vs)) }
        .context("CreateVertexShader failed")?;
    let mut ps = None;
    unsafe { device.CreatePixelShader(ps_bytes, None, Some(&mut ps)) }
        .context("CreatePixelShader failed")?;
    Ok((
        vs.context("CreateVertexShader produced no shader")?,
        ps.context("CreatePixelShader produced no shader")?,
    ))
}

fn compile_shader(entry_point: &str, target: &str) -> Result<ID3DBlob> {
    let entry = std::ffi::CString::new(entry_point).expect("no NUL in entry point");
    let target_c = std::ffi::CString::new(target).expect("no NUL in target");
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            SHADER_SOURCE.as_ptr() as *const c_void,
            SHADER_SOURCE.len(),
            None,
            None,
            None,
            windows::core::PCSTR(entry.as_ptr() as *const u8),
            windows::core::PCSTR(target_c.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let message = errors
            .map(|blob| unsafe {
                let ptr = blob.GetBufferPointer() as *const u8;
                let len = blob.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
            })
            .unwrap_or_default();
        bail!("D3DCompile({entry_point}) failed: {e} {message}");
    }
    code.context("D3DCompile produced no blob")
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut sampler = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut sampler)) }
        .context("CreateSamplerState failed")?;
    sampler.context("CreateSamplerState produced no state")
}

fn create_textures(device: &ID3D11Device, width: u32, height: u32) -> Result<Textures> {
    let uv_w = (width / 2).max(1);
    let uv_h = (height / 2).max(1);

    let luma_default = create_texture(
        device,
        width,
        height,
        DXGI_FORMAT_R8_UNORM,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
        0,
    )?;
    let luma_staging = create_texture(
        device,
        width,
        height,
        DXGI_FORMAT_R8_UNORM,
        D3D11_USAGE_STAGING,
        0,
        D3D11_CPU_ACCESS_WRITE.0 as u32,
    )?;
    let chroma_default = create_texture(
        device,
        uv_w,
        uv_h,
        DXGI_FORMAT_R8G8_UNORM,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
        0,
    )?;
    let chroma_staging = create_texture(
        device,
        uv_w,
        uv_h,
        DXGI_FORMAT_R8G8_UNORM,
        D3D11_USAGE_STAGING,
        0,
        D3D11_CPU_ACCESS_WRITE.0 as u32,
    )?;

    let mut luma_srv = None;
    unsafe { device.CreateShaderResourceView(&luma_default, None, Some(&mut luma_srv)) }
        .context("CreateShaderResourceView (luma) failed")?;
    let mut chroma_srv = None;
    unsafe { device.CreateShaderResourceView(&chroma_default, None, Some(&mut chroma_srv)) }
        .context("CreateShaderResourceView (chroma) failed")?;

    Ok(Textures {
        luma_default,
        luma_staging,
        luma_srv: luma_srv.context("no luma SRV")?,
        chroma_default,
        chroma_staging,
        chroma_srv: chroma_srv.context("no chroma SRV")?,
        width,
        height,
    })
}

#[allow(clippy::too_many_arguments)]
fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind_flags: u32,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D failed")?;
    texture.context("CreateTexture2D produced no texture")
}

fn upload_planes(
    context: &ID3D11DeviceContext,
    textures: &Textures,
    frame: &DecodedFrame,
) -> Result<()> {
    let width = frame.width as u32;
    let height = frame.height as u32;
    let uv_w = (width / 2).max(1);
    let uv_h = (height / 2).max(1);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(
            &textures.luma_staging,
            0,
            D3D11_MAP_WRITE,
            0,
            Some(&mut mapped),
        )
    }
    .context("Map (luma) failed")?;
    unsafe {
        copy_rows(
            mapped.pData as *mut u8,
            mapped.RowPitch,
            &frame.luma,
            width,
            height,
        )
    };
    unsafe { context.Unmap(&textures.luma_staging, 0) };
    unsafe { context.CopyResource(&textures.luma_default, &textures.luma_staging) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(
            &textures.chroma_staging,
            0,
            D3D11_MAP_WRITE,
            0,
            Some(&mut mapped),
        )
    }
    .context("Map (chroma) failed")?;
    // Chroma rows are uv_w pixels of 2 bytes each: uv_w * 2 == width for an
    // even width, which display sizes always are for 4:2:0 video.
    unsafe {
        copy_rows(
            mapped.pData as *mut u8,
            mapped.RowPitch,
            &frame.chroma,
            uv_w * 2,
            uv_h,
        )
    };
    unsafe { context.Unmap(&textures.chroma_staging, 0) };
    unsafe { context.CopyResource(&textures.chroma_default, &textures.chroma_staging) };

    Ok(())
}

unsafe fn copy_rows(dst_base: *mut u8, dst_pitch: u32, src: &[u8], row_bytes: u32, rows: u32) {
    for row in 0..rows as usize {
        let dst = dst_base.add(row * dst_pitch as usize);
        let src_off = row * row_bytes as usize;
        if src_off + row_bytes as usize > src.len() {
            break;
        }
        std::ptr::copy_nonoverlapping(src.as_ptr().add(src_off), dst, row_bytes as usize);
    }
}

/// An aspect-fit viewport, centred in the client area.
fn fit_viewport(
    client_width: u32,
    client_height: u32,
    content_width: u32,
    content_height: u32,
) -> D3D11_VIEWPORT {
    let client_aspect = client_width as f32 / client_height.max(1) as f32;
    let content_aspect = content_width as f32 / content_height.max(1) as f32;

    let (width, height) = if client_aspect > content_aspect {
        let h = client_height as f32;
        (h * content_aspect, h)
    } else {
        let w = client_width as f32;
        (w, w / content_aspect)
    };

    D3D11_VIEWPORT {
        TopLeftX: (client_width as f32 - width) * 0.5,
        TopLeftY: (client_height as f32 - height) * 0.5,
        Width: width,
        Height: height,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where `fit_viewport` puts 2:1 content in the client area, as
    /// (x, y, width, height).
    fn placement(client_width: u32, client_height: u32) -> (f32, f32, f32, f32) {
        let viewport = fit_viewport(client_width, client_height, 1280, 640);
        (
            viewport.TopLeftX,
            viewport.TopLeftY,
            viewport.Width,
            viewport.Height,
        )
    }

    #[test]
    fn a_wider_client_fills_the_height_and_centres_across() {
        assert_eq!(placement(1000, 400), (100.0, 0.0, 800.0, 400.0));
    }

    #[test]
    fn a_taller_client_fills_the_width_and_centres_down() {
        assert_eq!(placement(400, 400), (0.0, 100.0, 400.0, 200.0));
    }

    #[test]
    fn a_client_of_the_same_aspect_is_filled_exactly() {
        assert_eq!(placement(640, 320), (0.0, 0.0, 640.0, 320.0));
    }
}
