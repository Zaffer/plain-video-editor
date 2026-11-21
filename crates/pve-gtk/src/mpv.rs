//! A small, direct binding to libmpv: just what the editor uses.
//!
//! mpv's C client API has been stable for years; binding the dozen functions we
//! need keeps the build free of a bindings generator and of third-party API churn.

use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::ptr;

#[repr(C)]
struct MpvHandle {
    _private: [u8; 0],
}
#[repr(C)]
struct MpvRenderContext {
    _private: [u8; 0],
}

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvRenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

const FORMAT_STRING: c_int = 1;
const FORMAT_FLAG: c_int = 3;
const FORMAT_DOUBLE: c_int = 5;
const EVENT_NONE: c_int = 0;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const RENDER_PARAM_INVALID: c_int = 0;
const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;

#[link(name = "mpv")]
extern "C" {
    fn mpv_create() -> *mut MpvHandle;
    fn mpv_initialize(ctx: *mut MpvHandle) -> c_int;
    fn mpv_terminate_destroy(ctx: *mut MpvHandle);
    fn mpv_set_option_string(ctx: *mut MpvHandle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command_async(ctx: *mut MpvHandle, reply_userdata: u64, args: *mut *const c_char) -> c_int;
    fn mpv_set_property_async(ctx: *mut MpvHandle, reply_userdata: u64, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    fn mpv_get_property(ctx: *mut MpvHandle, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    fn mpv_observe_property(ctx: *mut MpvHandle, reply_userdata: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_wait_event(ctx: *mut MpvHandle, timeout: c_double) -> *mut MpvEvent;
    fn mpv_error_string(error: c_int) -> *const c_char;
    fn mpv_render_context_create(res: *mut *mut MpvRenderContext, mpv: *mut MpvHandle, params: *mut MpvRenderParam) -> c_int;
    fn mpv_render_context_render(ctx: *mut MpvRenderContext, params: *mut MpvRenderParam) -> c_int;
    fn mpv_render_context_set_update_callback(ctx: *mut MpvRenderContext, callback: Option<extern "C" fn(*mut c_void)>, callback_ctx: *mut c_void);
    fn mpv_render_context_report_swap(ctx: *mut MpvRenderContext);
    fn mpv_render_context_free(ctx: *mut MpvRenderContext);
}

#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

#[link(name = "GL")]
extern "C" {
    fn glGetIntegerv(pname: u32, params: *mut c_int);
    fn glBindFramebuffer(target: u32, framebuffer: u32);
    fn glReadPixels(x: c_int, y: c_int, width: c_int, height: c_int, format: u32, kind: u32, pixels: *mut c_void);
    fn glGenTextures(n: c_int, textures: *mut u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexImage2D(target: u32, level: c_int, internal: c_int, w: c_int, h: c_int, border: c_int, format: u32, kind: u32, data: *const c_void);
    fn glGenFramebuffers(n: c_int, framebuffers: *mut u32);
    fn glFramebufferTexture2D(target: u32, attachment: u32, textarget: u32, texture: u32, level: c_int);
    fn glDeleteFramebuffers(n: c_int, framebuffers: *const u32);
    fn glDeleteTextures(n: c_int, textures: *const u32);
}

/// Draw the current frame into a throw-away buffer and return its centre pixel (RGBA).
/// For tests: it works while the window is hidden, when GTK itself draws nothing.
/// A GL context must be current.
pub fn sample_offscreen(renderer: &Renderer) -> [u8; 4] {
    const GL_TEXTURE_2D: u32 = 0x0DE1;
    const GL_RGBA8: c_int = 0x8058;
    const GL_RGBA: u32 = 0x1908;
    const GL_UNSIGNED_BYTE: u32 = 0x1401;
    const GL_FRAMEBUFFER: u32 = 0x8D40;
    const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
    let (w, h) = (640, 360);
    let (mut tex, mut fbo, mut before) = (0u32, 0u32, 0 as c_int);
    // SAFETY: plain GL calls with a current context; every object created here is deleted again
    // and the previous framebuffer binding is restored.
    unsafe {
        glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut before);
        glGenTextures(1, &mut tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, ptr::null());
        glGenFramebuffers(1, &mut fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    }
    let drawn_into = renderer.render(w, h);
    let px = read_pixel(drawn_into, w / 2, h / 2);
    // SAFETY: as above.
    unsafe {
        glBindFramebuffer(GL_FRAMEBUFFER, before as u32);
        glDeleteFramebuffers(1, &fbo);
        glDeleteTextures(1, &tex);
    }
    px
}

/// The colour (RGBA) of one pixel of framebuffer `fbo`. For tests: it stalls the GPU pipeline.
/// The framebuffer is named explicitly because mpv leaves other framebuffers bound after drawing.
pub fn read_pixel(fbo: i32, x: i32, y: i32) -> [u8; 4] {
    const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
    const GL_RGBA: u32 = 0x1908;
    const GL_UNSIGNED_BYTE: u32 = 0x1401;
    let mut px = [0u8; 4];
    // SAFETY: a GL context is current; reads exactly one RGBA pixel into a 4-byte buffer.
    unsafe {
        glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo as u32);
        glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px.as_mut_ptr() as *mut c_void);
    }
    px
}

// eglGetProcAddress rather than libepoxy: epoxy on glvnd builds makes mpv report "unsupported".
extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: mpv passes a valid NUL-terminated symbol name.
    unsafe { eglGetProcAddress(name) }
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("no interior NUL")
}

fn error(code: c_int) -> String {
    // SAFETY: mpv_error_string returns a static string for any code.
    unsafe { CStr::from_ptr(mpv_error_string(code)) }.to_string_lossy().into_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A seek or load has finished and the new frame is ready.
    PlaybackRestart,
    /// An observed number changed (None when it became unavailable).
    Number(String, Option<f64>),
    /// An observed flag changed.
    Flag(String, bool),
}

/// One mpv instance. Dropping it shuts mpv down.
pub struct Mpv {
    handle: *mut MpvHandle,
}

impl Mpv {
    /// `options` are set before start-up (e.g. ("vo", "libmpv")).
    pub fn new(options: &[(&str, &str)]) -> Result<Mpv, String> {
        // mpv requires the C numeric locale.
        // SAFETY: setlocale with a static string; done on the main thread before mpv starts.
        unsafe { libc_setlocale_numeric_c() };
        // SAFETY: mpv_create has no preconditions; the handle is checked for null.
        let handle = unsafe { mpv_create() };
        if handle.is_null() {
            return Err("could not start mpv".into());
        }
        let mpv = Mpv { handle };
        for (name, value) in options {
            let (n, v) = (cstr(name), cstr(value));
            // SAFETY: the handle is valid and both strings outlive the call.
            let rc = unsafe { mpv_set_option_string(mpv.handle, n.as_ptr(), v.as_ptr()) };
            if rc < 0 {
                return Err(format!("mpv option {name}={value}: {}", error(rc)));
            }
        }
        // SAFETY: the handle is valid and not yet initialised.
        let rc = unsafe { mpv_initialize(mpv.handle) };
        if rc < 0 {
            return Err(format!("could not start mpv: {}", error(rc)));
        }
        Ok(mpv)
    }

    // Commands and property changes are queued, not waited for: a blocking call
    // costs a thread wake-up (about a millisecond) and stalls the interface whenever
    // mpv is busy. mpv runs them in the order they were sent.

    pub fn command(&self, args: &[&str]) {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(ptr::null());
        // SAFETY: a NULL-terminated array of valid C strings; mpv copies them before returning.
        unsafe { mpv_command_async(self.handle, 0, ptrs.as_mut_ptr()) };
    }

    pub fn set(&self, name: &str, value: &str) {
        let (n, v) = (cstr(name), cstr(value));
        let mut data: *const c_char = v.as_ptr();
        // SAFETY: FORMAT_STRING takes a pointer to a C string pointer; mpv copies the value before returning.
        unsafe { mpv_set_property_async(self.handle, 0, n.as_ptr(), FORMAT_STRING, &mut data as *mut *const c_char as *mut c_void) };
    }

    pub fn set_flag(&self, name: &str, on: bool) {
        self.set(name, if on { "yes" } else { "no" })
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let n = cstr(name);
        let mut value: c_double = 0.0;
        // SAFETY: FORMAT_DOUBLE writes one double into `value`.
        let rc = unsafe { mpv_get_property(self.handle, n.as_ptr(), FORMAT_DOUBLE, &mut value as *mut c_double as *mut c_void) };
        (rc >= 0).then_some(value)
    }

    pub fn observe_number(&self, name: &str) {
        let n = cstr(name);
        // SAFETY: the handle is valid; the name is copied by mpv.
        unsafe { mpv_observe_property(self.handle, 0, n.as_ptr(), FORMAT_DOUBLE) };
    }

    pub fn observe_flag(&self, name: &str) {
        let n = cstr(name);
        // SAFETY: as above.
        unsafe { mpv_observe_property(self.handle, 0, n.as_ptr(), FORMAT_FLAG) };
    }

    /// The next waiting event we care about, without blocking. None when the queue is empty.
    pub fn poll(&self) -> Option<Event> {
        loop {
            // SAFETY: the handle is valid; the returned event is valid until the next wait_event call,
            // and everything is copied out of it before returning.
            unsafe {
                let ev = &*mpv_wait_event(self.handle, 0.0);
                match ev.event_id {
                    EVENT_NONE => return None,
                    EVENT_PLAYBACK_RESTART => return Some(Event::PlaybackRestart),
                    EVENT_PROPERTY_CHANGE if !ev.data.is_null() => {
                        let p = &*(ev.data as *const MpvEventProperty);
                        let name = CStr::from_ptr(p.name).to_string_lossy().into_owned();
                        match p.format {
                            FORMAT_DOUBLE if !p.data.is_null() => return Some(Event::Number(name, Some(*(p.data as *const c_double)))),
                            FORMAT_FLAG if !p.data.is_null() => return Some(Event::Flag(name, *(p.data as *const c_int) != 0)),
                            _ => return Some(Event::Number(name, None)),
                        }
                    }
                    _ => continue,
                }
            }
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and never used again.
        unsafe { mpv_terminate_destroy(self.handle) };
    }
}

extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
}

unsafe fn libc_setlocale_numeric_c() {
    const LC_NUMERIC: c_int = 1; // Linux value
    setlocale(LC_NUMERIC, c"C".as_ptr());
}

/// mpv drawing into an OpenGL framebuffer that the caller owns.
///
/// Must be created, used and dropped with the same GL context current, and dropped before its `Mpv`.
pub struct Renderer {
    ctx: *mut MpvRenderContext,
    notify: *mut Box<dyn Fn() + Send + Sync>,
}

extern "C" fn on_update(data: *mut c_void) {
    // SAFETY: `data` is the Box<dyn Fn> leaked in Renderer::new, alive until Renderer is dropped,
    // which first unregisters this callback.
    let notify = unsafe { &*(data as *const Box<dyn Fn() + Send + Sync>) };
    notify();
}

impl Renderer {
    /// `on_frame` is called from an mpv thread whenever there is a new frame to draw.
    pub fn new(mpv: &Mpv, on_frame: impl Fn() + Send + Sync + 'static) -> Result<Renderer, String> {
        let mut init = MpvOpenglInitParams { get_proc_address, get_proc_address_ctx: ptr::null_mut() };
        let mut params = [
            MpvRenderParam { kind: RENDER_PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
            MpvRenderParam { kind: RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut init as *mut _ as *mut c_void },
            MpvRenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        let mut ctx: *mut MpvRenderContext = ptr::null_mut();
        // SAFETY: params is a valid INVALID-terminated array whose pointees live through the call.
        let rc = unsafe { mpv_render_context_create(&mut ctx, mpv.handle, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(format!("mpv cannot draw with OpenGL here: {}", error(rc)));
        }
        let boxed: Box<dyn Fn() + Send + Sync> = Box::new(on_frame);
        let notify = Box::into_raw(Box::new(boxed));
        // SAFETY: ctx is valid; `notify` stays alive until Drop unregisters the callback.
        unsafe { mpv_render_context_set_update_callback(ctx, Some(on_update), notify as *mut c_void) };
        Ok(Renderer { ctx, notify })
    }

    /// Draw the current frame into the framebuffer bound for drawing, sized w x h pixels.
    /// Returns that framebuffer's id.
    pub fn render(&self, w: i32, h: i32) -> i32 {
        let mut fbo_id: c_int = 0;
        // SAFETY: a GL context is current (we are inside the GLArea's render callback).
        unsafe { glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut fbo_id) };
        let mut fbo = MpvOpenglFbo { fbo: fbo_id, w, h, internal_format: 0 };
        let mut flip: c_int = 1;
        let mut params = [
            MpvRenderParam { kind: RENDER_PARAM_OPENGL_FBO, data: &mut fbo as *mut _ as *mut c_void },
            MpvRenderParam { kind: RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
            MpvRenderParam { kind: RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        // SAFETY: valid context and INVALID-terminated params; GL context is current.
        unsafe {
            mpv_render_context_render(self.ctx, params.as_mut_ptr());
            mpv_render_context_report_swap(self.ctx);
        }
        fbo_id
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: unregister the callback before freeing its data; then free the context
        // (the caller guarantees the GL context is current).
        unsafe {
            mpv_render_context_set_update_callback(self.ctx, None, ptr::null_mut());
            mpv_render_context_free(self.ctx);
            drop(Box::from_raw(self.notify));
        }
    }
}
