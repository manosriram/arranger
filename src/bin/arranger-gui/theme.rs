//! Graphite (dark) and Paper (light), tokens from docs/ui/Themes.dc.html.

pub type Rgba = [u8; 4];

const fn hex(v: u32) -> Rgba {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]
}

/// `a` moved `k` (0..1) of the way towards `b`.
pub fn blend(a: Rgba, b: Rgba, k: f32) -> Rgba {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k) as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]), 255]
}

pub fn alpha(c: Rgba, a: u8) -> Rgba {
    [c[0], c[1], c[2], a]
}

pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    pub bg: Rgba,
    pub panel: Rgba,
    pub term: Rgba,
    pub raised: Rgba,
    pub border: Rgba,
    pub border2: Rgba,
    pub text: Rgba,
    pub text2: Rgba,
    pub text3: Rgba,
    pub faint: Rgba,
    pub sel: Rgba,
    pub sel_ring: Rgba,
    pub tc: Rgba,
    pub tc_on: Rgba,
    pub primary_bg: Rgba,
    pub primary_fg: Rgba,
    pub on_accent: Rgba,
    pub add: Rgba,
    pub del: Rgba,
    pub needs: Rgba,
    pub work: Rgba,
    pub done: Rgba,
    /// Dialog surface (NewTask mockup).
    pub dialog: Rgba,
    pub scrim: Rgba,
}

pub const GRAPHITE: Theme = Theme {
    name: "Graphite",
    dark: true,
    bg: hex(0x0B0C0E),
    panel: hex(0x0D0E10),
    term: hex(0x090A0C),
    raised: hex(0x1A1D22),
    border: hex(0x1D2025),
    border2: hex(0x2A2E35),
    text: hex(0xE8EAED),
    text2: hex(0xA3A9B2),
    text3: hex(0x7D848E),
    faint: hex(0x4E555E),
    sel: hex(0x16191D),
    sel_ring: hex(0x262A30),
    tc: hex(0xC9CDD3),
    tc_on: hex(0xF3F4F6),
    primary_bg: hex(0xE8EAED),
    primary_fg: hex(0x0B0C0E),
    on_accent: hex(0x0B0C0E),
    add: hex(0x7EE2A8),
    del: hex(0xFF8F80),
    needs: hex(0xFFB547),
    work: hex(0x6AA8FF),
    done: hex(0x5FAE84),
    dialog: hex(0x0F1113),
    scrim: [4, 5, 6, 184],
};

pub const PAPER: Theme = Theme {
    name: "Paper",
    dark: false,
    bg: hex(0xFFFFFF),
    panel: hex(0xF7F7F8),
    term: hex(0xFBFBFC),
    raised: hex(0xEDEEF0),
    border: hex(0xE4E5E8),
    border2: hex(0xD3D5D9),
    text: hex(0x16181C),
    text2: hex(0x454A52),
    text3: hex(0x646A73),
    faint: hex(0xA3A8AF),
    sel: hex(0xECEDF0),
    sel_ring: hex(0xD9DBDF),
    tc: hex(0x2B2F35),
    tc_on: hex(0x0B0C0E),
    primary_bg: hex(0x16181C),
    primary_fg: hex(0xFFFFFF),
    on_accent: hex(0xFFFFFF),
    add: hex(0x16794A),
    del: hex(0xC2412D),
    needs: hex(0xB25B00),
    work: hex(0x2160E0),
    done: hex(0x2F7D55),
    dialog: hex(0xFFFFFF),
    scrim: [16, 24, 40, 90],
};

pub const AGENTS: [&str; 4] = ["claude", "codex", "pi", "gemini"];

pub fn accent(agent: &str) -> Rgba {
    match agent {
        "claude" => hex(0xF0875A),
        "codex" => hex(0x4FD1B8),
        "pi" => hex(0xB794F6),
        "gemini" => hex(0x7FB0FF),
        _ => hex(0x7D848E),
    }
}

impl Theme {
    /// ANSI 0-15, derived from the theme's own tokens (the mockups define no terminal palette).
    pub fn ansi(&self, i: u8) -> Rgba {
        let (black, white) = if self.dark { (self.raised, self.text2) } else { (self.text2, self.raised) };
        let base = [black, self.del, self.add, self.needs, self.work, accent("pi"), accent("codex"), white];
        let bright = [self.faint, self.del, self.add, self.needs, self.work, accent("pi"), accent("codex"), self.text];
        if i < 8 { base[i as usize] } else { bright[(i - 8) as usize % 8] }
    }

    /// xterm 256-colour palette above the first 16.
    pub fn indexed(&self, i: u8) -> Rgba {
        match i {
            0..16 => self.ansi(i),
            16..232 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [level(i / 36), level(i / 6 % 6), level(i % 6), 255]
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                [v, v, v, 255]
            }
        }
    }
}
