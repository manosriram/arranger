//! One batched quad renderer: rounded rects (SDF) and glyphs from a swash-rasterised atlas.
//! All public coordinates are logical pixels; `dpi` scales to the framebuffer.

use super::theme::Rgba;
use glow::HasContext;
use std::collections::HashMap;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Cap, Fill, Format, Mask, Stroke, Transform};
use swash::{FontRef, NormalizedCoord};

pub const UI: usize = 0;
pub const MONO: usize = 1;
const BUNDLED: [&[u8]; 2] = [include_bytes!("../../../assets/fonts/Geist.ttf"), include_bytes!("../../../assets/fonts/GeistMono.ttf")];
/// System fonts tried, in order, for glyphs Geist lacks (❯ ⏺ ⎿ ✳ …). Missing files are skipped.
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

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    atlas: glow::Texture,
    screen_loc: Option<glow::UniformLocation>,
    verts: Vec<Vertex>,
    scaler: ScaleContext,
    glyphs: HashMap<GlyphKey, Option<Glyph>>,
    shelf: (i32, i32, i32), // x, y, row height
    pub dpi: f32,
    size: (u32, u32),
    clip: Option<Rect>,
}

/// Added to every requested weight: Geist at its nominal weights reads too thin on screen.
const WEIGHT_BOOST: f32 = 100.0;

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
                screen_loc,
                verts: Vec::new(),
                scaler: ScaleContext::new(),
                glyphs: HashMap::new(),
                shelf: (0, 0, 0),
                dpi: 1.0,
                size: (1, 1),
                clip: None,
            })
        }
    }

    pub fn begin(&mut self, width: u32, height: u32, dpi: f32, clear: Rgba) {
        self.size = (width.max(1), height.max(1));
        self.dpi = dpi;
        unsafe {
            self.gl.viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
            let c = clear.map(|v| v as f32 / 255.0);
            self.gl.clear_color(c[0], c[1], c[2], 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    pub fn finish(&mut self) {
        self.flush();
        self.set_clip(None);
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

    /// Filled rect with corner radius.
    pub fn rect(&mut self, r: Rect, radius: f32, color: Rgba) {
        let d = self.dpi;
        // Round both edges, not the size, so neighbouring rects (terminal cell backgrounds) meet without seams.
        let (x, y) = ((r.x * d).round(), (r.y * d).round());
        let (w, h) = (((r.x + r.w) * d).round() - x, ((r.y + r.h) * d).round() - y);
        self.quad(x, y, w, h, [0.0; 4], (radius * d).min(w / 2.0).min(h / 2.0).max(0.0), color);
    }

    /// Rect with a 1px border of `line` (drawn as an outer rect behind an inset fill).
    pub fn frame(&mut self, r: Rect, radius: f32, fill: Rgba, line: Rgba) {
        self.rect(r, radius, line);
        self.rect(r.inset(1.0), (radius - 1.0).max(0.0), fill);
    }

    /// (ascent, descent) in logical px.
    pub fn vmetrics(&self, font_i: usize, size: f32) -> (f32, f32) {
        let f = font(font_i);
        let m = f.metrics(&coords(&f, 400)).scale(size);
        (m.ascent, m.descent)
    }

    /// Top y that vertically centres a line of `size` text in `r`.
    pub fn center(&self, r: Rect, font_i: usize, size: f32) -> f32 {
        let (a, d) = self.vmetrics(font_i, size);
        r.y + (r.h - (a + d)) / 2.0
    }

    pub fn advance(&self, font_i: usize, size: f32, weight: u16, ch: char) -> f32 {
        let (fi, id) = resolve(font_i, ch);
        let f = font(fi);
        f.glyph_metrics(&coords(&f, weight)).scale(size).advance_width(id)
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
            if total > max_w && pen + adv > x + max_w - ellipsis {
                self.glyph_char(font_i, '…', pen, baseline, size, weight, color);
                return pen + ellipsis - x;
            }
            self.glyph_char(font_i, ch, pen, baseline, size, weight, color);
            pen += adv;
        }
        pen - x
    }

    pub fn glyph_char(&mut self, font_i: usize, ch: char, x: f32, baseline: f32, size: f32, weight: u16, color: Rgba) {
        let (fi, id) = resolve(font_i, ch);
        if id != 0 {
            self.glyph(fi, id, x, baseline, size, weight, color);
        }
    }

    fn glyph(&mut self, font_i: usize, id: u16, x: f32, baseline: f32, size: f32, weight: u16, color: Rgba) {
        let d = self.dpi;
        let key = GlyphKey { font: font_i, id, px: (size * d * 64.0) as u32, weight };
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let g = self.rasterize(key);
                self.glyphs.insert(key, g);
                g
            }
        };
        if let Some(g) = g {
            let (px, py) = ((x * d).round() + g.left, (baseline * d).round() - g.top);
            self.quad(px, py, g.w, g.h, g.uv, -1.0, color);
        }
    }

    /// Draws an SVG path laid out in a 64-unit box, `size` logical px square at (x, y): filled, or
    /// stroked `stroke` units wide with round caps. Rasterised once per (id, size) into the atlas.
    pub fn path(&mut self, id: u16, data: &str, stroke: Option<f32>, x: f32, y: f32, size: f32, color: Rgba) {
        let d = self.dpi;
        let key = GlyphKey { font: usize::MAX, id, px: (size * d * 64.0) as u32, weight: 0 };
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let scale = size * d / 64.0;
                let mut mask = Mask::new(data);
                mask.transform(Some(Transform::scale(scale, scale)));
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
        let img = Render::new(&[Source::Outline]).format(Format::Alpha).render(&mut scaler, key.id)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_coverage() {
        for ch in ['■', '●', '○', '✓', '›', '↵', '→', '─', '│', '❯', '⏺', '✳', '⎿', '…', '⌘', '⇧'] {
            println!("{ch} ui={} mono={}", Renderer::has_glyph(UI, ch), Renderer::has_glyph(MONO, ch));
        }
    }
}
