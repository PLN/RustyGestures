//! One click-through DirectComposition window per monitor.
//!
//! Windows are created hidden and shown only while a stroke is captured.
//! Per-monitor V2 DPI is set by the caller before the first window exists.
//! `WM_DISPLAYCHANGE` and `WM_DPICHANGED` ask the main loop to rebuild or resize.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEBUG_LEVEL_NONE,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_UNIT_MODE_PIXELS,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain, IDXGISwapChain1,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, RegisterClassExW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, UnregisterClassW, HMENU, HTTRANSPARENT,
    LWA_ALPHA, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_NCHITTEST, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows_numerics::Vector2;

struct UiFlags {
    rebuild: AtomicBool,
    resize: AtomicBool,
}

static UI_FLAGS: AtomicPtr<UiFlags> = AtomicPtr::new(std::ptr::null_mut());

struct Gpu {
    device: ID3D11Device,
    d2d_device: ID2D1Device,
    dcomp: IDCompositionDevice,
    factory: IDXGIFactory2,
}

struct MonitorWindow {
    hwnd: HWND,
    origin: POINT,
    swap: IDXGISwapChain1,
    context: ID2D1DeviceContext,
    target: ID2D1RenderTarget,
    bitmap: Option<ID2D1Bitmap1>,
    // Held so the composition visual and target stay alive for the window.
    #[allow(dead_code)]
    visual: IDCompositionVisual,
    #[allow(dead_code)]
    composition: IDCompositionTarget,
    pixel_width: u32,
    pixel_height: u32,
    shown: bool,
}

pub(crate) struct Overlay {
    gpu: Option<Gpu>,
    windows: Vec<MonitorWindow>,
    class_atom: u16,
    instance: HINSTANCE,
    flags: Box<UiFlags>,
    visible: bool,
}

impl Overlay {
    pub(crate) fn new() -> windows::core::Result<Self> {
        let module = unsafe { GetModuleHandleW(None)? };
        let instance = HINSTANCE(module.0);
        let flags = Box::new(UiFlags {
            rebuild: AtomicBool::new(false),
            resize: AtomicBool::new(false),
        });
        UI_FLAGS.store(&*flags as *const UiFlags as *mut UiFlags, Ordering::Release);
        let class_atom = register_class(instance)?;
        let gpu = match Gpu::new() {
            Ok(gpu) => Some(gpu),
            Err(err) => {
                eprintln!("rustygestures: trail unavailable ({err})");
                None
            }
        };
        let mut overlay = Self {
            gpu,
            windows: Vec::new(),
            class_atom,
            instance,
            flags,
            visible: false,
        };
        if overlay.gpu.is_some() {
            if let Err(err) = overlay.rebuild() {
                eprintln!("rustygestures: trail windows were not created ({err})");
            }
        }
        Ok(overlay)
    }

    pub(crate) fn trail_active(&self) -> bool {
        !self.windows.is_empty()
    }

    pub(crate) fn sync(&mut self, show: bool, points: &[(i32, i32)]) {
        if self.flags.rebuild.swap(false, Ordering::AcqRel) {
            self.hide_all();
            self.windows.clear();
            if let Err(err) = self.rebuild() {
                eprintln!("rustygestures: display change rebuild failed ({err})");
            }
        }
        if self.flags.resize.swap(false, Ordering::AcqRel) {
            for window in &mut self.windows {
                if let Err(err) = window.resize_to_client() {
                    eprintln!("rustygestures: trail resize failed ({err})");
                }
            }
        }
        if show == self.visible && !show {
            return;
        }
        if show {
            let dcomp = self.gpu.as_ref().map(|gpu| gpu.dcomp.clone());
            if let Some(dcomp) = dcomp {
                for window in &mut self.windows {
                    let local = local_points(points, window.origin);
                    if let Err(err) = window.paint(&dcomp, &local) {
                        eprintln!("rustygestures: trail paint failed ({err})");
                    }
                    window.set_shown(true);
                }
            }
        } else {
            self.hide_all();
        }
        self.visible = show;
    }

    fn hide_all(&mut self) {
        for window in &mut self.windows {
            window.set_shown(false);
        }
        self.visible = false;
    }

    fn rebuild(&mut self) -> windows::core::Result<()> {
        let Some(gpu) = self.gpu.as_ref() else {
            return Ok(());
        };
        for monitor in monitors() {
            let window = MonitorWindow::create(gpu, self.instance, &monitor)?;
            self.windows.push(window);
        }
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.hide_all();
        self.windows.clear();
        UI_FLAGS.store(std::ptr::null_mut(), Ordering::Release);
        unsafe {
            let _ = UnregisterClassW(
                PCWSTR(self.class_atom as usize as *const u16),
                Some(self.instance),
            );
        }
    }
}

struct MonitorRect {
    origin: POINT,
    width: i32,
    height: i32,
}

fn monitors() -> Vec<MonitorRect> {
    let mut found = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum),
            LPARAM(&mut found as *mut Vec<MonitorRect> as isize),
        );
    }
    found
}

unsafe extern "system" fn monitor_enum(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let list = &mut *(data.0 as *mut Vec<MonitorRect>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let rect = info.rcMonitor;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width > 0 && height > 0 {
            list.push(MonitorRect {
                origin: POINT {
                    x: rect.left,
                    y: rect.top,
                },
                width,
                height,
            });
        }
    }
    windows::core::BOOL(1)
}

fn register_class(instance: HINSTANCE) -> windows::core::Result<u16> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance,
        lpszClassName: w!("RustyGesturesTrail"),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        Err(windows::core::Error::from_thread())
    } else {
        Ok(atom)
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            if let Some(flags) = flags() {
                flags.rebuild.store(true, Ordering::Release);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let rect = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            if let Some(flags) = flags() {
                flags.resize.store(true, Ordering::Release);
            }
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn flags() -> Option<&'static UiFlags> {
    let ptr = UI_FLAGS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

impl Gpu {
    fn new() -> windows::core::Result<Self> {
        let device = create_d3d_device()?;
        let dxgi: IDXGIDevice = device.cast()?;
        let options = D2D1_FACTORY_OPTIONS {
            debugLevel: D2D1_DEBUG_LEVEL_NONE,
        };
        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))? };
        let d2d_device = unsafe { d2d_factory.CreateDevice(&dxgi)? };
        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi)? };
        let adapter = unsafe { dxgi.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        Ok(Self {
            device,
            d2d_device,
            dcomp,
            factory,
        })
    }
}

fn create_d3d_device() -> windows::core::Result<ID3D11Device> {
    let levels = [
        windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0,
        windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0,
    ];
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        if result.is_ok() {
            if let Some(device) = device {
                return Ok(device);
            }
        }
    }
    Err(windows::core::Error::from_thread())
}

impl MonitorWindow {
    fn create(
        gpu: &Gpu,
        instance: HINSTANCE,
        monitor: &MonitorRect,
    ) -> windows::core::Result<Self> {
        let ex_style = WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_TOPMOST
            | WS_EX_NOACTIVATE
            | WS_EX_TOOLWINDOW
            | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                w!("RustyGesturesTrail"),
                w!("RustyGestures"),
                WS_POPUP,
                monitor.origin.x,
                monitor.origin.y,
                monitor.width,
                monitor.height,
                None,
                None::<HMENU>,
                Some(instance),
                None,
            )?
        };
        unsafe {
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
        }
        let width = monitor.width as u32;
        let height = monitor.height as u32;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
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
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swap = unsafe {
            gpu.factory
                .CreateSwapChainForComposition(&gpu.device, &desc, None)?
        };
        let context = unsafe {
            gpu.d2d_device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };
        unsafe { context.SetUnitMode(D2D1_UNIT_MODE_PIXELS) };
        let target: ID2D1RenderTarget = context.cast()?;
        let visual = unsafe { gpu.dcomp.CreateVisual()? };
        unsafe { visual.SetContent(&swap)? };
        let composition = unsafe { gpu.dcomp.CreateTargetForHwnd(hwnd, true)? };
        unsafe {
            composition.SetRoot(&visual)?;
            gpu.dcomp.Commit()?;
        }
        let mut window = Self {
            hwnd,
            origin: monitor.origin,
            swap,
            context,
            target,
            bitmap: None,
            visual,
            composition,
            pixel_width: width,
            pixel_height: height,
            shown: false,
        };
        window.bind_bitmap()?;
        Ok(window)
    }

    fn bind_bitmap(&mut self) -> windows::core::Result<()> {
        let base: IDXGISwapChain = self.swap.cast()?;
        let surface: IDXGISurface = unsafe { base.GetBuffer(0)? };
        let props = bitmap_props();
        let bitmap = unsafe {
            self.context
                .CreateBitmapFromDxgiSurface(&surface, Some(&props))?
        };
        unsafe { self.context.SetTarget(&bitmap) };
        self.bitmap = Some(bitmap);
        Ok(())
    }

    fn resize_to_client(&mut self) -> windows::core::Result<()> {
        let mut rect = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rect)? };
        let width = (rect.right - rect.left).max(1) as u32;
        let height = (rect.bottom - rect.top).max(1) as u32;
        if width == self.pixel_width && height == self.pixel_height && self.bitmap.is_some() {
            return Ok(());
        }
        self.bitmap = None;
        unsafe { self.context.SetTarget(None::<&ID2D1Image>) };
        let base: IDXGISwapChain = self.swap.cast()?;
        unsafe { base.ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, Default::default())? };
        self.pixel_width = width;
        self.pixel_height = height;
        self.bind_bitmap()?;
        Ok(())
    }

    fn paint(
        &mut self,
        dcomp: &IDCompositionDevice,
        points: &[(f32, f32)],
    ) -> windows::core::Result<()> {
        if self.bitmap.is_none() {
            self.bind_bitmap()?;
        }
        let dpi = unsafe { GetDpiForWindow(self.hwnd) }.max(96) as f32;
        let width = (2.0 * dpi / 96.0).max(1.0);
        unsafe {
            self.target.BeginDraw();
            self.target.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            if points.len() >= 2 {
                let color = D2D1_COLOR_F {
                    r: 0.86,
                    g: 0.38,
                    b: 0.12,
                    a: 0.92,
                };
                let brush: ID2D1SolidColorBrush =
                    self.target.CreateSolidColorBrush(&color, None)?;
                for pair in points.windows(2) {
                    self.target.DrawLine(
                        Vector2 {
                            X: pair[0].0,
                            Y: pair[0].1,
                        },
                        Vector2 {
                            X: pair[1].0,
                            Y: pair[1].1,
                        },
                        &brush,
                        width,
                        None,
                    );
                }
            }
            self.target.EndDraw(None, None)?;
        }
        let base: IDXGISwapChain = self.swap.cast()?;
        let hr = unsafe { base.Present(1, Default::default()) };
        hr.ok()?;
        unsafe { dcomp.Commit()? };
        Ok(())
    }

    fn set_shown(&mut self, show: bool) {
        if self.shown == show {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        self.shown = show;
    }
}

impl Drop for MonitorWindow {
    fn drop(&mut self) {
        self.bitmap = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn bitmap_props() -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        ..Default::default()
    }
}

fn local_points(points: &[(i32, i32)], origin: POINT) -> Vec<(f32, f32)> {
    points
        .iter()
        .map(|(x, y)| ((*x - origin.x) as f32, (*y - origin.y) as f32))
        .collect()
}
