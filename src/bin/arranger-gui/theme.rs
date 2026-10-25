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
    /// A needs-you card's fill and border.
    pub needs_bg: Rgba,
    pub needs_line: Rgba,
    pub work: Rgba,
    pub done: Rgba,
    /// Dialog surface (NewTask mockup).
    pub dialog: Rgba,
    pub scrim: Rgba,
}

pub const GRUVBOX: Theme = Theme {
    name: "Gruvbox",
    dark: true,
    bg: hex(0x282828),
    panel: hex(0x282828),
    term: hex(0x1D2021),
    raised: hex(0x3C3836),
    border: hex(0x3C3836),
    border2: hex(0x504945),
    row_line: hex(0x32302F),
    text: hex(0xEBDBB2),
    text2: hex(0xD5C4A1),
    text3: hex(0xA89984),
    faint: hex(0x665C54),
    sel: hex(0x3C3836),
    sel_ring: hex(0x504945),
    tc: hex(0xD5C4A1),
    tc_on: hex(0xFBF1C7),
    primary_bg: hex(0xEBDBB2),
    primary_fg: hex(0x282828),
    on_accent: hex(0x282828),
    add: hex(0xB8BB26),
    del: hex(0xFB4934),
    needs: hex(0xFABD2F),
    needs_bg: hex(0x32302F),
    needs_line: hex(0x6B5A2A),
    work: hex(0x83A598),
    done: hex(0xB8BB26),
    dialog: hex(0x32302F),
    scrim: [0, 0, 0, 150],
};

pub const INK: Theme = Theme {
    name: "Ink",
    dark: true,
    bg: hex(0x101318),
    panel: hex(0x101318),
    term: hex(0x0B0D11),
    raised: hex(0x1C212A),
    border: hex(0x20252E),
    border2: hex(0x2D3440),
    row_line: hex(0x171B22),
    text: hex(0xE6E9EF),
    text2: hex(0xB4BBC7),
    text3: hex(0x8A93A3),
    faint: hex(0x4A5261),
    sel: hex(0x1C212A),
    sel_ring: hex(0x2D3440),
    tc: hex(0xC9CED8),
    tc_on: hex(0xF3F5F8),
    primary_bg: hex(0xE6E9EF),
    primary_fg: hex(0x101318),
    on_accent: hex(0x101318),
    add: hex(0x7EDBA8),
    del: hex(0xFF8F80),
    needs: hex(0xF2B35B),
    needs_bg: hex(0x1A1813),
    needs_line: hex(0x4A3B22),
    work: hex(0x7DA6FF),
    done: hex(0x6CC59A),
    dialog: hex(0x14181F),
    scrim: [0, 0, 0, 150],
};

pub const MOSS: Theme = Theme {
    name: "Moss",
    dark: true,
    bg: hex(0x151915),
    panel: hex(0x151915),
    term: hex(0x101310),
    raised: hex(0x242A24),
    border: hex(0x232923),
    border2: hex(0x323A32),
    row_line: hex(0x1B201B),
    text: hex(0xE3E8DD),
    text2: hex(0xB6BEAE),
    text3: hex(0x8E9887),
    faint: hex(0x4E574A),
    sel: hex(0x242A24),
    sel_ring: hex(0x323A32),
    tc: hex(0xCCD3C5),
    tc_on: hex(0xF1F4EE),
    primary_bg: hex(0xE3E8DD),
    primary_fg: hex(0x151915),
    on_accent: hex(0x151915),
    add: hex(0xA6D68E),
    del: hex(0xE8907C),
    needs: hex(0xE8B85A),
    needs_bg: hex(0x1D1C14),
    needs_line: hex(0x4A4226),
    work: hex(0x86B3D9),
    done: hex(0x97C77F),
    dialog: hex(0x1A1F1A),
    scrim: [0, 0, 0, 150],
};

pub const EMBER: Theme = Theme {
    name: "Ember",
    dark: true,
    bg: hex(0x1A1715),
    panel: hex(0x1A1715),
    term: hex(0x141210),
    raised: hex(0x2C2723),
    border: hex(0x2A2622),
    border2: hex(0x3A3430),
    row_line: hex(0x211D1A),
    text: hex(0xF0EAE2),
    text2: hex(0xC4BBB0),
    text3: hex(0x9A9086),
    faint: hex(0x5A524B),
    sel: hex(0x2C2723),
    sel_ring: hex(0x3A3430),
    tc: hex(0xD8D0C6),
    tc_on: hex(0xFAF6F0),
    primary_bg: hex(0xF0EAE2),
    primary_fg: hex(0x1A1715),
    on_accent: hex(0x1A1715),
    add: hex(0x9CD6AC),
    del: hex(0xF09080),
    needs: hex(0xF0A848),
    needs_bg: hex(0x221C14),
    needs_line: hex(0x55401F),
    work: hex(0x82B1E3),
    done: hex(0x8CC59E),
    dialog: hex(0x201C19),
    scrim: [0, 0, 0, 150],
};

pub const GRUVBOX_LIGHT: Theme = Theme {
    name: "Gruvbox Light",
    dark: false,
    bg: hex(0xF2E5BC),
    panel: hex(0xF2E5BC),
    term: hex(0xFBF1C7),
    raised: hex(0xEBDBB2),
    border: hex(0xE0CFA5),
    border2: hex(0xD5C4A1),
    row_line: hex(0xEBDFB8),
    text: hex(0x3C3836),
    text2: hex(0x504945),
    text3: hex(0x665C54),
    faint: hex(0xA89984),
    sel: hex(0xEBDBB2),
    sel_ring: hex(0xD5C4A1),
    tc: hex(0x3C3836),
    tc_on: hex(0x282828),
    primary_bg: hex(0x3C3836),
    primary_fg: hex(0xFBF1C7),
    on_accent: hex(0xFBF1C7),
    add: hex(0x79740E),
    del: hex(0x9D0006),
    needs: hex(0x8F5A0A),
    needs_bg: hex(0xF9EAB8),
    needs_line: hex(0xD9B45E),
    work: hex(0x076678),
    done: hex(0x79740E),
    dialog: hex(0xFBF1C7),
    scrim: [40, 40, 40, 77],
};

pub const SNOW: Theme = Theme {
    name: "Snow",
    dark: false,
    bg: hex(0xF3F5F8),
    panel: hex(0xF3F5F8),
    term: hex(0xFFFFFF),
    raised: hex(0xE7EBF0),
    border: hex(0xDDE2E9),
    border2: hex(0xC9D0DA),
    row_line: hex(0xECEFF3),
    text: hex(0x151C26),
    text2: hex(0x3F4A57),
    text3: hex(0x5C6876),
    faint: hex(0xAAB3BE),
    sel: hex(0xE7EBF0),
    sel_ring: hex(0xC9D0DA),
    tc: hex(0x26303C),
    tc_on: hex(0x151C26),
    primary_bg: hex(0x151C26),
    primary_fg: hex(0xFFFFFF),
    on_accent: hex(0xFFFFFF),
    add: hex(0x1F7A4D),
    del: hex(0xB53A2C),
    needs: hex(0x9A5800),
    needs_bg: hex(0xFFF7EA),
    needs_line: hex(0xF0D6A8),
    work: hex(0x1E5FCC),
    done: hex(0x1F7A4D),
    dialog: hex(0xFFFFFF),
    scrim: [21, 28, 38, 64],
};

pub const PAPER: Theme = Theme {
    name: "Paper",
    dark: false,
    bg: hex(0xF6F5F2),
    panel: hex(0xF6F5F2),
    term: hex(0xFFFFFF),
    raised: hex(0xECEBE6),
    border: hex(0xE4E2DC),
    border2: hex(0xD6D3CB),
    row_line: hex(0xEEECE7),
    text: hex(0x1B1B19),
    text2: hex(0x45453F),
    text3: hex(0x66655F),
    faint: hex(0xB5B3AB),
    sel: hex(0xECEBE6),
    sel_ring: hex(0xD6D3CB),
    tc: hex(0x2E2E2B),
    tc_on: hex(0x1B1B19),
    primary_bg: hex(0x1B1B19),
    primary_fg: hex(0xFFFFFF),
    on_accent: hex(0xFFFFFF),
    add: hex(0x2F7D4F),
    del: hex(0xB4412F),
    needs: hex(0x9A5B00),
    needs_bg: hex(0xFFF8EC),
    needs_line: hex(0xEFD7AA),
    work: hex(0x1F5FD1),
    done: hex(0x2F7D4F),
    dialog: hex(0xFFFFFF),
    scrim: [27, 27, 25, 64],
};

pub const LINEN: Theme = Theme {
    name: "Linen",
    dark: false,
    bg: hex(0xF1ECE2),
    panel: hex(0xF1ECE2),
    term: hex(0xFAF7F1),
    raised: hex(0xE6DFD1),
    border: hex(0xDED5C4),
    border2: hex(0xCCC1AC),
    row_line: hex(0xE9E2D5),
    text: hex(0x2A251F),
    text2: hex(0x51493F),
    text3: hex(0x6B6153),
    faint: hex(0xB0A693),
    sel: hex(0xE6DFD1),
    sel_ring: hex(0xCCC1AC),
    tc: hex(0x3A332B),
    tc_on: hex(0x2A251F),
    primary_bg: hex(0x2A251F),
    primary_fg: hex(0xFAF7F1),
    on_accent: hex(0xFAF7F1),
    add: hex(0x3D7A3A),
    del: hex(0xA3402F),
    needs: hex(0x8A5000),
    needs_bg: hex(0xFBF1DE),
    needs_line: hex(0xE6CC9C),
    work: hex(0x2C5E99),
    done: hex(0x3D7A3A),
    dialog: hex(0xFAF7F1),
    scrim: [42, 37, 31, 64],
};

/// Dark ones first; the scheme menu lists them in this order. ⌘T swaps each dark scheme for the
/// light one four places on (Gruvbox ↔ Gruvbox Light, Ink ↔ Snow, Moss ↔ Paper, Ember ↔ Linen).
pub const THEMES: [&Theme; 8] = [&GRUVBOX, &INK, &MOSS, &EMBER, &GRUVBOX_LIGHT, &SNOW, &PAPER, &LINEN];

/// Index into THEMES for a saved scheme name; names from before the current eight map to their
/// nearest one, and anything else to Gruvbox.
pub fn by_name(name: &str) -> usize {
    let name = match name {
        "Graphite" | "Midnight" => "Ink",
        "Mist" => "Snow",
        "Sage" => "Paper",
        n => n,
    };
    THEMES.iter().position(|t| t.name == name).unwrap_or(0)
}

pub const AGENTS: [&str; 4] = ["claude", "codex", "pi", "gemini"];

/// Models the new-task dialog offers per agent: (label, `--model` value); "default" passes none.
pub fn models(agent: &str) -> &'static [(&'static str, &'static str)] {
    match agent {
        "claude" => &[
            ("default", ""),
            ("opus 5.5", "claude-opus-5-5"),
            ("sonnet 5.5", "claude-sonnet-5-5"),
            ("haiku 5.5", "claude-haiku-5-5"),
            ("fable 5.1", "claude-fable-5-1"),
        ],
        "codex" => &[
            ("default", ""),
            ("gpt-6.1-sol", "gpt-6.1-sol"),
            ("gpt-6-astra", "gpt-6-astra"),
            ("gpt-6-sol", "gpt-6-sol"),
            ("gpt-6-luna", "gpt-6-luna"),
        ],
        // pi takes a pattern, matched against whichever providers it's logged in to.
        "pi" => &[
            ("default", ""),
            ("opus 5.5", "claude-opus-5-5"),
            ("sonnet 5.5", "claude-sonnet-5-5"),
            ("gpt-6.1-sol", "gpt-6.1-sol"),
        ],
        // gemini's own aliases, so they follow its releases.
        "gemini" => &[("default", ""), ("pro", "pro"), ("flash", "flash"), ("flash-lite", "flash-lite")],
        _ => &[("default", "")],
    }
}

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
