//! One batched quad renderer: rounded rects (SDF) and glyphs from a swash-rasterised atlas.
//! All public coordinates are logical pixels; `dpi` scales to the framebuffer.

use super::theme::Rgba;
use glow::HasContext;
use std::cell::RefCell;
use std::collections::HashMap;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Angle, Cap, Fill, Format, Mask, Stroke, Transform, Vector};
use swash::{FontRef, NormalizedCoord};

pub const UI: usize = 0;
pub const MONO: usize = 1;
/// Inter for interface text; JetBrains Mono for code and the terminal. Both have variable weights.
/// Inter source: google/fonts@9862abc066c9b83bf84c3e8ed1fc2d8d48f9bbf0/ofl/inter (SIL OFL).
const INTER: &[u8] = include_bytes!("../../../assets/fonts/Inter.ttf");
const JETBRAINS: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono.ttf");
const BUNDLED: [&[u8]; 2] = [INTER, JETBRAINS];
/// System fonts tried, in order, for glyphs the bundled faces lack (❯ ⏺ ⎿ ✳ …). Missing files are skipped.
const FALLBACKS: &[&str] = &[
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
    "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
];
const ATLAS: i32 = 2048;

const VS: &str = r#"#version 330 core
layout(location = 0) in vec2 pos;
layout(location = 1) in vec2 uv;
layout(location = 2) in vec2 local;
layout(location = 3) in vec2 half_size;
layout(location = 4) in float radius;
layout(location = 5) in vec4 color;
uniform vec2 screen;
out vec2 v_uv; out vec2 v_local; out vec2 v_half; out float v_radius; out vec4 v_color;
void main() {
    gl_Position = vec4(pos.x / screen.x * 2.0 - 1.0, 1.0 - pos.y / screen.y * 2.0, 0.0, 1.0);
    v_uv = uv; v_local = local; v_half = half_size; v_radius = radius; v_color = color;
}"#;

const FS: &str = r#"#version 330 core
in vec2 v_uv; in vec2 v_local; in vec2 v_half; in float v_radius; in vec4 v_color;
uniform sampler2D atlas;
out vec4 frag;
void main() {
    float a;
    if (v_radius < -1.5) {
        frag = texture(atlas, v_uv);
        return;
    }
    if (v_radius < 0.0) {
        a = texture(atlas, v_uv).r;
    } else {
        vec2 q = abs(v_local) - v_half + v_radius;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - v_radius;
        a = clamp(0.5 - d, 0.0, 1.0);
    }
    frag = vec4(v_color.rgb, v_color.a * a);
}"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    local: [f32; 2],
    half: [f32; 2],
    radius: f32, // < 0 marks a glyph
    color: Rgba,
}

#[derive(Clone, Copy)]
struct Glyph {
    uv: [f32; 4],
    left: f32,
    top: f32,
    w: f32,
    h: f32,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct GlyphKey {
    font: usize,
    id: u16,
    px: u32, // physical size * 64
    weight: u16,
    sub: u8, // x offset in quarter pixels, so glyph spacing stays even
    rot: u16, // path icons only: rotation in whole degrees
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
}

/// The mouse cursor over a click target.
#[derive(Clone, Copy, PartialEq)]
pub enum Cursor {
    Arrow,
    Pointer,
    Text,
    /// A divider that drags left and right.
    Resize,
}

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    atlas: glow::Texture,
    /// Decoded images by path, with the file stamp they were decoded from.
    images: HashMap<std::path::PathBuf, (Option<FileStamp>, Option<(glow::Texture, u32, u32)>)>,
    screen_loc: Option<glow::UniformLocation>,
    verts: Vec<Vertex>,
    scaler: ScaleContext,
    glyphs: HashMap<GlyphKey, Option<Glyph>>,
    shelf: (i32, i32, i32), // x, y, row height
    pub dpi: f32,
    size: (u32, u32),
    clip: Option<Rect>,
    /// Layout lookups, cached: parsing the font and its variation tables on every call was most
    /// of a frame in text-heavy views (the diff). Keys: (font, char), (font, char, weight, size), (font, size).
    resolved: RefCell<HashMap<(usize, char), (usize, u16)>>,
    advances: RefCell<HashMap<(usize, char, u16, u32), f32>>,
    vmetrics: RefCell<HashMap<(usize, u32), (f32, f32)>>,
    /// This frame's click targets in draw order, with the clip they were drawn under.
    targets: Vec<(Rect, Option<Rect>, Cursor)>,
    /// Offscreen copy of the last full frame (framebuffer, its texture, size), so an animation
    /// tick can copy it back and draw only what moved. None if the driver can't make one.
    layer: Option<(glow::Framebuffer, glow::Texture, (u32, u32))>,
}

/// A small weight lift keeps interface and terminal text readable at small sizes.
const WEIGHT_BOOST: f32 = 50.0;

fn coords(font: &FontRef, weight: u16) -> Vec<NormalizedCoord> {
    font.variations().normalized_coords([("wght", weight as f32 + WEIGHT_BOOST)]).collect()
}

fn fonts() -> &'static [&'static [u8]] {
    static FONTS: std::sync::OnceLock<Vec<&'static [u8]>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        let system = FALLBACKS.iter().filter_map(|p| std::fs::read(p).ok()).map(|d| &*Box::leak(d.into_boxed_slice()));
        BUNDLED.into_iter().chain(system).collect()
    })
}

fn font(i: usize) -> FontRef<'static> {
    FontRef::from_index(fonts()[i], 0).expect("font")
}

/// First font, starting with `font_i`, that has `ch`: (font index, glyph id).
fn resolve(font_i: usize, ch: char) -> (usize, u16) {
    let id = font(font_i).charmap().map(ch);
    if id != 0 || ch.is_ascii() {
        return (font_i, id);
    }
    (BUNDLED.len()..fonts().len())
        .find_map(|i| Some(font(i).charmap().map(ch)).filter(|&g| g != 0).map(|g| (i, g)))
        .unwrap_or((font_i, 0))
}

impl Renderer {
    pub fn new(gl: glow::Context) -> Result<Self, String> {
        unsafe {
            let program = gl.create_program()?;
            for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, src);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            let vao = gl.create_vertex_array()?;
            let vbo = gl.create_buffer()?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = std::mem::size_of::<Vertex>() as i32;
            for (loc, n, off) in [(0, 2, 0), (1, 2, 8), (2, 2, 16), (3, 2, 24), (4, 1, 32)] {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, n, glow::FLOAT, false, stride, off);
            }
            gl.enable_vertex_attrib_array(5);
            gl.vertex_attrib_pointer_f32(5, 4, glow::UNSIGNED_BYTE, true, stride, 36);

            let atlas = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,
                ATLAS,
                ATLAS,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&vec![0u8; (ATLAS * ATLAS) as usize])),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            let screen_loc = gl.get_uniform_location(program, "screen");
            Ok(Self {
                gl,
                program,
                vao,
                vbo,
                atlas,
                images: HashMap::new(),
                screen_loc,
                verts: Vec::new(),
                scaler: ScaleContext::new(),
                glyphs: HashMap::new(),
                shelf: (0, 0, 0),
                dpi: 1.0,
                size: (1, 1),
                clip: None,
                resolved: RefCell::default(),
                advances: RefCell::default(),
                vmetrics: RefCell::default(),
                targets: Vec::new(),
                layer: None,
            })
        }
    }

    /// Starts a full frame, drawn into the layer until `finish` copies it to the window.
    pub fn begin(&mut self, width: u32, height: u32, dpi: f32, clear: Rgba) {
        self.size = (width.max(1), height.max(1));
        self.dpi = dpi;
        self.targets.clear();
        unsafe {
            if self.layer.is_none_or(|l| l.2 != self.size) {
                self.make_layer();
            }
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, self.layer.map(|l| l.0));
            self.gl.viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
            let c = clear.map(|v| v as f32 / 255.0);
            self.gl.clear_color(c[0], c[1], c[2], 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// Draws the hover tint on the target under `mouse` and returns its cursor.
    pub fn finish(&mut self, mouse: (f32, f32)) -> Cursor {
        let hot = self.hot(mouse).map(|i| self.targets[i]);
        if let Some((r, clip, Cursor::Pointer)) = hot {
            self.set_clip(clip);
            // Mid grey: lightens dark buttons and darkens light ones, in either theme.
            self.rect(r, 4.0, [128, 128, 128, 0x24]);
        }
        self.set_clip(None);
        self.present();
        hot.map_or(Cursor::Arrow, |t| t.2)
    }

    /// Starts an animation tick: the last full frame goes back on the window as is, for the
    /// moving parts to draw over. False when there's no layer of this size; draw a full frame.
    pub fn reuse(&mut self, width: u32, height: u32) -> bool {
        if self.layer.is_none_or(|l| l.2 != (width, height)) {
            return false;
        }
        unsafe { self.gl.viewport(0, 0, width as i32, height as i32) };
        self.present();
        true
    }

    /// Draws what's queued since `finish` or `reuse` onto the window.
    pub fn end(&mut self) {
        self.flush();
        self.set_clip(None);
    }

    /// Flushes into the layer and copies it to the window; drawing goes to the window after.
    fn present(&mut self) {
        self.flush();
        let Some((fb, _, (w, h))) = self.layer else { return };
        unsafe {
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fb));
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            let (w, h) = (w as i32, h as i32);
            self.gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// (Re)makes the layer at the current size.
    fn make_layer(&mut self) {
        let gl = &self.gl;
        unsafe {
            if let Some((fb, tex, _)) = self.layer.take() {
                gl.delete_framebuffer(fb);
                gl.delete_texture(tex);
            }
            let (Ok(fb), Ok(tex)) = (gl.create_framebuffer(), gl.create_texture()) else { return };
            let (w, h) = (self.size.0 as i32, self.size.1 as i32);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fb));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE {
                self.layer = Some((fb, tex, self.size));
            } else {
                gl.delete_framebuffer(fb);
                gl.delete_texture(tex);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// The clip drawing is under now.
    pub fn clip(&self) -> Option<Rect> {
        self.clip
    }

    /// Registers a click target; the topmost one under the mouse gets the hover tint and its cursor.
    pub fn target(&mut self, r: Rect, cursor: Cursor) {
        self.targets.push((r, self.clip, cursor));
    }

    /// Whether `p` falls inside the current clip, so a row scrolled out of view can't be clicked.
    pub fn in_clip(&self, p: (f32, f32)) -> bool {
        self.clip.is_none_or(|c| c.contains(p))
    }

    /// Index of the topmost target under `p` in the last frame.
    pub fn hot(&self, p: (f32, f32)) -> Option<usize> {
        self.targets.iter().rposition(|(r, clip, _)| r.contains(p) && clip.is_none_or(|c| c.contains(p)))
    }

    /// Dev aid: writes the frame as a binary PPM (`ARRANGER_SNAPSHOT=file`).
    pub fn snapshot(&self, path: &str) -> std::io::Result<()> {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let mut px = vec![0u8; w * h * 3];
        unsafe {
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(0, 0, w as i32, h as i32, glow::RGB, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        }
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for row in px.chunks(w * 3).rev() {
            out.extend_from_slice(row);
        }
        std::fs::write(path, out)
    }

    fn flush(&mut self) {
        if self.verts.is_empty() {
            return;
        }
        let gl = &self.gl;
        unsafe {
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(self.screen_loc.as_ref(), self.size.0 as f32, self.size.1 as f32);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(
                self.verts.as_ptr() as *const u8,
                self.verts.len() * std::mem::size_of::<Vertex>(),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas));
            gl.draw_arrays(glow::TRIANGLES, 0, self.verts.len() as i32);
        }
        self.verts.clear();
    }

    /// Restricts drawing to `clip` (logical px) until the next call.
    pub fn set_clip(&mut self, clip: Option<Rect>) {
        if clip == self.clip {
            return;
        }
        self.flush();
        self.clip = clip;
        unsafe {
            match clip {
                Some(r) => {
                    let d = self.dpi;
                    let y = self.size.1 as f32 - (r.y + r.h) * d;
                    self.gl.enable(glow::SCISSOR_TEST);
                    self.gl.scissor((r.x * d) as i32, y as i32, (r.w * d).ceil() as i32, (r.h * d).ceil() as i32);
                }
                None => self.gl.disable(glow::SCISSOR_TEST),
            }
        }
    }

    fn quad(&mut self, x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], radius: f32, color: Rgba) {
        let (hw, hh) = (w / 2.0, h / 2.0);
        let v = |px: f32, py: f32, u: f32, vv: f32| Vertex {
            pos: [x + px * w, y + py * h],
            uv: [u, vv],
            local: [(px - 0.5) * w, (py - 0.5) * h],
            half: [hw, hh],
            radius,
            color,
        };
        let [u0, v0, u1, v1] = uv;
        let (a, b, c, d) = (v(0.0, 0.0, u0, v0), v(1.0, 0.0, u1, v0), v(0.0, 1.0, u0, v1), v(1.0, 1.0, u1, v1));
        self.verts.extend([a, b, c, b, d, c]);
    }

    /// Filled rect. Corners are square throughout the app, so `_radius` is not drawn.
    pub fn rect(&mut self, r: Rect, _radius: f32, color: Rgba) {
        let d = self.dpi;
        // Round both edges, not the size, so neighbouring rects (terminal cell backgrounds) meet without seams.
        let (x, y) = ((r.x * d).round(), (r.y * d).round());
        let (w, h) = (((r.x + r.w) * d).round() - x, ((r.y + r.h) * d).round() - y);
        self.quad(x, y, w, h, [0.0; 4], 0.0, color);
    }

    /// Draw a local image inside a bounding box, preserving its aspect ratio.
    /// Failed decodes are cached too, so invalid attachments do not retry every frame. A file
    /// rewritten in place (a re-rendered screenshot) is decoded again.
    pub fn image(&mut self, path: &std::path::Path, bounds: Rect) -> bool {
        let stamp = file_stamp(path);
        if let Some((old, entry)) = self.images.get(path)
            && *old != stamp {
            if let Some((texture, _, _)) = *entry {
                self.flush();
                unsafe { self.gl.delete_texture(texture); }
            }
            self.images.remove(path);
        }
        if !self.images.contains_key(path) {
            // Bound GPU memory when browsing long conversations.
            if self.images.len() >= 64 {
                self.flush();
                for (_, (_, entry)) in self.images.drain() {
                    if let Some((texture, _, _)) = entry {
                        unsafe { self.gl.delete_texture(texture); }
                    }
                }
            }
            let loaded = (|| {
                let mut reader = image::ImageReader::open(path).ok()?;
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(16384);
                limits.max_image_height = Some(16384);
                limits.max_alloc = Some(128 * 1024 * 1024);
                reader.limits(limits);
                let pixels = reader.decode().ok()?.thumbnail(1600, 1600).to_rgba8();
                let (w, h) = pixels.dimensions();
                self.flush();
                unsafe {
                    let gl = &self.gl;
                    let texture = gl.create_texture().ok()?;
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(pixels.as_raw())));
                    Some((texture, w, h))
                }
            })();
            self.images.insert(path.to_path_buf(), (stamp, loaded));
        }
        let Some((texture, w, h)) = self.images[path].1 else { return false };
        let scale = (bounds.w / w as f32).min(bounds.h / h as f32).min(1.0);
        self.flush();
        let atlas = self.atlas;
        self.atlas = texture;
        let d = self.dpi;
        self.quad(bounds.x * d, bounds.y * d, w as f32 * scale * d, h as f32 * scale * d, [0.0, 0.0, 1.0, 1.0], -2.0, [255; 4]);
        self.flush();
        self.atlas = atlas;
        true
    }

    /// Rect with a 1px border of `line` (drawn as an outer rect behind an inset fill).
    pub fn frame(&mut self, r: Rect, radius: f32, fill: Rgba, line: Rgba) {
        self.rect(r, radius, line);
        self.rect(r.inset(1.0), (radius - 1.0).max(0.0), fill);
    }

    /// (ascent, descent) in logical px.
    pub fn vmetrics(&self, font_i: usize, size: f32) -> (f32, f32) {
        *self.vmetrics.borrow_mut().entry((font_i, size.to_bits())).or_insert_with(|| {
            let f = font(font_i);
            let m = f.metrics(&coords(&f, 400)).scale(size);
            (m.ascent, m.descent)
        })
    }

    /// Top y that vertically centres a line of `size` text in `r`.
    pub fn center(&self, r: Rect, font_i: usize, size: f32) -> f32 {
        let (a, d) = self.vmetrics(font_i, size);
        r.y + (r.h - (a + d)) / 2.0
    }

    pub fn advance(&self, font_i: usize, size: f32, weight: u16, ch: char) -> f32 {
        if let Some(&a) = self.advances.borrow().get(&(font_i, ch, weight, size.to_bits())) {
            return a;
        }
        let (fi, id) = self.resolve(font_i, ch);
        let f = font(fi);
        let a = f.glyph_metrics(&coords(&f, weight)).scale(size).advance_width(id);
        self.advances.borrow_mut().insert((font_i, ch, weight, size.to_bits()), a);
        a
    }

    fn resolve(&self, font_i: usize, ch: char) -> (usize, u16) {
        *self.resolved.borrow_mut().entry((font_i, ch)).or_insert_with(|| resolve(font_i, ch))
    }

    pub fn measure(&self, font_i: usize, size: f32, weight: u16, s: &str) -> f32 {
        s.chars().map(|ch| self.advance(font_i, size, weight, ch)).sum()
    }

    #[cfg(test)]
    fn has_glyph(font_i: usize, ch: char) -> bool {
        resolve(font_i, ch).1 != 0
    }

    /// Draws `s` with its top at `y`; returns the width. Text past `max_w` is cut with an ellipsis.
    pub fn text(&mut self, x: f32, y: f32, s: &str, font_i: usize, size: f32, weight: u16, color: Rgba, max_w: f32) -> f32 {
        let ellipsis = self.advance(font_i, size, weight, '…');
        let total = self.measure(font_i, size, weight, s);
        let baseline = y + self.vmetrics(font_i, size).0;
        let mut pen = x;
        for ch in s.chars() {
            let adv = self.advance(font_i, size, weight, ch);
            // Subtracting an absolute pen position can lose a fraction of a pixel when
            // callers allocate exactly the measured width (for example inline metadata).
            if total > max_w + 0.01 && pen + adv > x + max_w - ellipsis {
                self.glyph_char(font_i, '…', pen, baseline, size, weight, color);
                return pen + ellipsis - x;
            }
            self.glyph_char(font_i, ch, pen, baseline, size, weight, color);
            pen += adv;
        }
        pen - x
    }

    pub fn glyph_char(&mut self, font_i: usize, ch: char, x: f32, baseline: f32, size: f32, weight: u16, color: Rgba) {
        let (fi, id) = self.resolve(font_i, ch);
        if id != 0 {
            self.glyph(fi, id, x, baseline, size, weight, color);
        }
    }

    fn glyph(&mut self, font_i: usize, id: u16, x: f32, baseline: f32, size: f32, weight: u16, color: Rgba) {
        let d = self.dpi;
        let (mut col, mut sub) = ((x * d).floor(), ((x * d).fract() * 4.0).round() as u8);
        if sub == 4 {
            (col, sub) = (col + 1.0, 0);
        }
        let key = GlyphKey { font: font_i, id, px: (size * d * 64.0) as u32, weight, sub, rot: 0 };
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let g = self.rasterize(key);
                self.glyphs.insert(key, g);
                g
            }
        };
        if let Some(g) = g {
            let (px, py) = (col + g.left, (baseline * d).round() - g.top);
            self.quad(px, py, g.w, g.h, g.uv, -1.0, color);
        }
    }

    /// Draws an SVG path laid out in a 64-unit box, `size` logical px square at (x, y): filled, or
    /// stroked `stroke` units wide with round caps. Rasterised once per (id, size) into the atlas.
    pub fn path(&mut self, id: u16, data: &str, stroke: Option<f32>, x: f32, y: f32, size: f32, color: Rgba) {
        self.path_rot(id, data, 0.0, stroke, x, y, size, color);
    }

    /// Like `path`, but rotates the icon (drawn in a 64-unit box centred on 32,32) clockwise by `angle` degrees.
    pub fn path_rot(&mut self, id: u16, data: &str, angle: f32, stroke: Option<f32>, x: f32, y: f32, size: f32, color: Rgba) {
        let d = self.dpi;
        let rot = angle.rem_euclid(360.0).round() as u16;
        let key = GlyphKey { font: usize::MAX, id, px: (size * d * 64.0) as u32, weight: 0, rot, sub: 0 };
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let scale = size * d / 64.0;
                let mut mask = Mask::new(data);
                // Not Transform::rotation_about: zeno 0.3 applies its translations in the wrong
                // order, so the icon orbits the origin instead of turning in place.
                let turn = Transform::translation(-32.0, -32.0)
                    .then_rotate(Angle::from_degrees(angle))
                    .then_translate(32.0, 32.0)
                    .then(&Transform::scale(scale, scale));
                mask.transform(Some(turn));
                match stroke {
                    Some(w) => mask.style(*Stroke::new(w).cap(Cap::Round)),
                    None => mask.style(Fill::NonZero),
                };
                let (img, at) = mask.render();
                let g = self.upload(at.width as i32, at.height as i32, &img).map(|uv| Glyph {
                    uv,
                    left: at.left as f32,
                    top: -at.top as f32,
                    w: at.width as f32,
                    h: at.height as f32,
                });
                self.glyphs.insert(key, g);
                g
            }
        };
        if let Some(g) = g {
            self.quad((x * d).round() + g.left, (y * d).round() - g.top, g.w, g.h, g.uv, -1.0, color);
        }
    }

    fn rasterize(&mut self, key: GlyphKey) -> Option<Glyph> {
        let f = font(key.font);
        let c = coords(&f, key.weight);
        let mut scaler = self.scaler.builder(f).size(key.px as f32 / 64.0).hint(true).normalized_coords(&c).build();
        let img = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .offset(Vector::new(key.sub as f32 / 4.0, 0.0))
            .render(&mut scaler, key.id)?;
        let p = img.placement;
        let uv = self.upload(p.width as i32, p.height as i32, &img.data)?;
        Some(Glyph { uv, left: p.left as f32, top: p.top as f32, w: p.width as f32, h: p.height as f32 })
    }

    /// Copies an alpha bitmap into the atlas; returns its uv rect.
    fn upload(&mut self, w: i32, h: i32, data: &[u8]) -> Option<[f32; 4]> {
        if w == 0 || h == 0 {
            return None;
        }
        let (mut x, mut y, mut row) = self.shelf;
        if x + w + 1 > ATLAS {
            (x, y, row) = (0, y + row + 1, 0);
        }
        if y + h + 1 > ATLAS {
            // ponytail: full atlas starts over; an LRU would keep hot glyphs if this ever churns
            self.flush();
            self.glyphs.clear();
            (x, y, row) = (0, 0, 0);
        }
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x,
                y,
                w,
                h,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
        }
        self.shelf = (x + w + 1, y, row.max(h));
        let s = ATLAS as f32;
        Some([x as f32 / s, y as f32 / s, (x + w) as f32 / s, (y + h) as f32 / s])
    }
}

/// When a file was last written and how big it is: what says a cached decode is stale.
type FileStamp = (std::time::SystemTime, u64);

fn file_stamp(path: &std::path::Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_coverage() {
        for ch in ['■', '●', '○', '✓', '›', '↵', '→', '─', '│', '❯', '⏺', '✳', '⎿', '…', '⌘', '⇧'] {
            println!("{ch} ui={} mono={}", Renderer::has_glyph(UI, ch), Renderer::has_glyph(MONO, ch));
        }
    }

    #[test]
    fn rewriting_an_image_changes_its_stamp() {
        let dir = std::env::temp_dir().join(format!("arranger-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        assert_eq!(file_stamp(&path), None, "a missing file has no stamp");
        std::fs::write(&path, b"first").unwrap();
        let first = file_stamp(&path).unwrap();
        assert_eq!(file_stamp(&path), Some(first), "unchanged, the cached decode stays");
        std::fs::write(&path, b"second, longer").unwrap();
        assert_ne!(file_stamp(&path), Some(first), "rewritten in place, it's decoded again");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
