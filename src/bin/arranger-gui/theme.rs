//! Four dark and four light colour schemes, tokens from the Themes mockup.

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

fn luminance(c: Rgba) -> f32 {
    let l = |v: u8| (v as f32 / 255.0).powf(2.2);
    0.2126 * l(c[0]) + 0.7152 * l(c[1]) + 0.0722 * l(c[2])
}

/// `fg`, pushed towards black or white until it reads on `bg` (contrast ≥ 3:1), like a terminal's
/// minimum-contrast setting. Agents pick colours for a dark terminal; their white text vanishes on Paper.
pub fn readable(fg: Rgba, bg: Rgba) -> Rgba {
    let lb = luminance(bg);
    let ok = |c: Rgba| {
        let l = luminance(c);
        (l.max(lb) + 0.05) / (l.min(lb) + 0.05) >= 3.0
    };
    if ok(fg) {
        return fg;
    }
    // Greys flip (white → black, light grey → dark grey) so their emphasis order survives.
    let neutral = fg.iter().take(3).max().unwrap() - fg.iter().take(3).min().unwrap() < 16;
    let fg = if neutral { [255 - fg[0], 255 - fg[1], 255 - fg[2], fg[3]] } else { fg };
    if ok(fg) {
        return fg;
    }
    let target = if lb > 0.18 { [0, 0, 0, 255] } else { [255; 4] };
    (1..=10).map(|i| blend(fg, target, i as f32 / 10.0)).find(|&c| ok(c)).unwrap_or(target)
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
    /// Between table rows.
    pub row_line: Rgba,
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
    row_line: hex(0x15181B),
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

pub const MIDNIGHT: Theme = Theme {
    name: "Midnight",
    dark: true,
    bg: hex(0x0E121A),
    panel: hex(0x111622),
    term: hex(0x0B0F16),
    raised: hex(0x1B2232),
    border: hex(0x1C2331),
    border2: hex(0x2A3345),
    row_line: hex(0x151B27),
    text: hex(0xE3E8F2),
    text2: hex(0xA1AABB),
    text3: hex(0x7B8597),
    faint: hex(0x4C5568),
    sel: hex(0x172031),
    sel_ring: hex(0x26324A),
    tc: hex(0xC6CEDC),
    tc_on: hex(0xF2F5FA),
    primary_bg: hex(0xE3E8F2),
    primary_fg: hex(0x0E121A),
    on_accent: hex(0x0E121A),
    add: hex(0x7FD8A6),
    del: hex(0xF59A8C),
    needs: hex(0xF5B44A),
    work: hex(0x7AA7FF),
    done: hex(0x64B08A),
    dialog: hex(0x141A2A),
    scrim: [4, 5, 6, 184],
};

pub const MOSS: Theme = Theme {
    name: "Moss",
    dark: true,
    bg: hex(0x111411),
    panel: hex(0x141814),
    term: hex(0x0E110E),
    raised: hex(0x1E241E),
    border: hex(0x1F251F),
    border2: hex(0x2E362E),
    row_line: hex(0x181D18),
    text: hex(0xE4E8DF),
    text2: hex(0xA8B1A2),
    text3: hex(0x808A7A),
    faint: hex(0x4F584B),
    sel: hex(0x1A201A),
    sel_ring: hex(0x2B342B),
    tc: hex(0xC9D0C2),
    tc_on: hex(0xF1F4EC),
    primary_bg: hex(0xE4E8DF),
    primary_fg: hex(0x111411),
    on_accent: hex(0x111411),
    add: hex(0x9BD48A),
    del: hex(0xEE9A84),
    needs: hex(0xE8B85C),
    work: hex(0x84B4E8),
    done: hex(0x7FB98C),
    dialog: hex(0x171C17),
    scrim: [4, 5, 6, 184],
};

pub const EMBER: Theme = Theme {
    name: "Ember",
    dark: true,
    bg: hex(0x15120F),
    panel: hex(0x191512),
    term: hex(0x110F0C),
    raised: hex(0x241F1A),
    border: hex(0x26201B),
    border2: hex(0x362E27),
    row_line: hex(0x1D1814),
    text: hex(0xEDE6DC),
    text2: hex(0xB5AA9C),
    text3: hex(0x8C8173),
    faint: hex(0x5A5047),
    sel: hex(0x201B16),
    sel_ring: hex(0x332B23),
    tc: hex(0xD2C8BA),
    tc_on: hex(0xF7F1E8),
    primary_bg: hex(0xEDE6DC),
    primary_fg: hex(0x15120F),
    on_accent: hex(0x15120F),
    add: hex(0xA9CF86),
    del: hex(0xEE9076),
    needs: hex(0xF0B35A),
    work: hex(0x8DB2E6),
    done: hex(0x8DB67A),
    dialog: hex(0x1D1815),
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
    row_line: hex(0xF0F1F3),
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

pub const LINEN: Theme = Theme {
    name: "Linen",
    dark: false,
    bg: hex(0xFBF8F2),
    panel: hex(0xF4F0E7),
    term: hex(0xF8F5EE),
    raised: hex(0xECE6DA),
    border: hex(0xE5DED1),
    border2: hex(0xD4CBBB),
    row_line: hex(0xEFE9DE),
    text: hex(0x24201A),
    text2: hex(0x4E473D),
    text3: hex(0x6B6357),
    faint: hex(0xA69D8F),
    sel: hex(0xEEE8DC),
    sel_ring: hex(0xDAD2C3),
    tc: hex(0x3A342B),
    tc_on: hex(0x1A1712),
    primary_bg: hex(0x24201A),
    primary_fg: hex(0xFBF8F2),
    on_accent: hex(0xFFFFFF),
    add: hex(0x2E7A3E),
    del: hex(0xB5432C),
    needs: hex(0xA85A00),
    work: hex(0x2B5FC2),
    done: hex(0x3C7A4E),
    dialog: hex(0xFBF8F2),
    scrim: [16, 24, 40, 90],
};

pub const MIST: Theme = Theme {
    name: "Mist",
    dark: false,
    bg: hex(0xF7F9FB),
    panel: hex(0xEFF3F7),
    term: hex(0xF4F7FA),
    raised: hex(0xE4EAF1),
    border: hex(0xDDE3EA),
    border2: hex(0xCBD3DD),
    row_line: hex(0xE9EEF3),
    text: hex(0x18202B),
    text2: hex(0x445063),
    text3: hex(0x5F6B7D),
    faint: hex(0x9AA5B4),
    sel: hex(0xE6ECF3),
    sel_ring: hex(0xD0D9E4),
    tc: hex(0x2A3442),
    tc_on: hex(0x0E151F),
    primary_bg: hex(0x18202B),
    primary_fg: hex(0xF7F9FB),
    on_accent: hex(0xFFFFFF),
    add: hex(0x1C7A50),
    del: hex(0xBF3F33),
    needs: hex(0xA65A00),
    work: hex(0x1F5FD6),
    done: hex(0x2F7A5B),
    dialog: hex(0xF7F9FB),
    scrim: [16, 24, 40, 90],
};

pub const SAGE: Theme = Theme {
    name: "Sage",
    dark: false,
    bg: hex(0xF7F9F5),
    panel: hex(0xEEF2EB),
    term: hex(0xF4F7F1),
    raised: hex(0xE3E9DF),
    border: hex(0xDCE3D8),
    border2: hex(0xC8D1C3),
    row_line: hex(0xE8EDE5),
    text: hex(0x1C231B),
    text2: hex(0x46503F),
    text3: hex(0x5E6A57),
    faint: hex(0x9BA693),
    sel: hex(0xE6ECE2),
    sel_ring: hex(0xCFD8CA),
    tc: hex(0x2D362B),
    tc_on: hex(0x121810),
    primary_bg: hex(0x1C231B),
    primary_fg: hex(0xF7F9F5),
    on_accent: hex(0xFFFFFF),
    add: hex(0x2D7A3A),
    del: hex(0xB5442F),
    needs: hex(0x9E5A00),
    work: hex(0x2C5FB8),
    done: hex(0x2F7B47),
    dialog: hex(0xF7F9F5),
    scrim: [16, 24, 40, 90],
};

/// Dark ones first; the scheme menu lists them in this order.
pub const THEMES: [&Theme; 8] = [&GRAPHITE, &MIDNIGHT, &MOSS, &EMBER, &PAPER, &LINEN, &MIST, &SAGE];

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

/// The agent colour deep enough to fill a button under white text on a light theme.
pub fn accent_deep(agent: &str) -> Rgba {
    match agent {
        "claude" => hex(0xC4552B),
        "codex" => hex(0x0B7F69),
        "pi" => hex(0x7445C7),
        "gemini" => hex(0x2C66D6),
        _ => hex(0x565C66),
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
