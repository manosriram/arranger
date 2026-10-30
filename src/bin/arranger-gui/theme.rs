//! Six dark and six light colour schemes.

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

pub const OCEAN: Theme = Theme {
    name: "Ocean",
    dark: true,
    bg: hex(0x111D28),
    panel: hex(0x111D28),
    term: hex(0x0B151F),
    raised: hex(0x203343),
    border: hex(0x263B4C),
    border2: hex(0x38536A),
    row_line: hex(0x192936),
    text: hex(0xE0EEF5),
    text2: hex(0xB1C9D8),
    text3: hex(0x87A6BA),
    faint: hex(0x48677C),
    sel: hex(0x203343),
    sel_ring: hex(0x38536A),
    tc: hex(0xC5DCE9),
    tc_on: hex(0xF0F8FC),
    primary_bg: hex(0xE0EEF5),
    primary_fg: hex(0x111D28),
    on_accent: hex(0x111D28),
    add: hex(0x80D4B0),
    del: hex(0xF3938D),
    needs: hex(0xEBC375),
    needs_bg: hex(0x242A27),
    needs_line: hex(0x5E5335),
    work: hex(0x7DBDEB),
    done: hex(0x80D4B0),
    dialog: hex(0x172734),
    scrim: [0, 0, 0, 150],
};

pub const PLUM: Theme = Theme {
    name: "Plum",
    dark: true,
    bg: hex(0x211824),
    panel: hex(0x211824),
    term: hex(0x18111B),
    raised: hex(0x35283B),
    border: hex(0x3C2D42),
    border2: hex(0x55405D),
    row_line: hex(0x2A202F),
    text: hex(0xF0E4F2),
    text2: hex(0xCDB8D1),
    text3: hex(0xA78DAD),
    faint: hex(0x6D5273),
    sel: hex(0x35283B),
    sel_ring: hex(0x55405D),
    tc: hex(0xDDC8E1),
    tc_on: hex(0xFBF1FC),
    primary_bg: hex(0xF0E4F2),
    primary_fg: hex(0x211824),
    on_accent: hex(0x211824),
    add: hex(0xA2D5AA),
    del: hex(0xF29BAA),
    needs: hex(0xE9C17D),
    needs_bg: hex(0x30262A),
    needs_line: hex(0x65513C),
    work: hex(0xB8A9EF),
    done: hex(0xA2D5AA),
    dialog: hex(0x2B202F),
    scrim: [0, 0, 0, 150],
};

pub const SEAFOAM: Theme = Theme {
    name: "Seafoam",
    dark: false,
    bg: hex(0xEDF5F2),
    panel: hex(0xEDF5F2),
    term: hex(0xF8FCFA),
    raised: hex(0xDDEBE5),
    border: hex(0xD1E2DA),
    border2: hex(0xB5CFC2),
    row_line: hex(0xE3EEE8),
    text: hex(0x19382F),
    text2: hex(0x3B5B4F),
    text3: hex(0x567568),
    faint: hex(0x9AB5A7),
    sel: hex(0xDDEBE5),
    sel_ring: hex(0xB5CFC2),
    tc: hex(0x284C3E),
    tc_on: hex(0x19382F),
    primary_bg: hex(0x19382F),
    primary_fg: hex(0xF8FCFA),
    on_accent: hex(0xF8FCFA),
    add: hex(0x276D49),
    del: hex(0xB13F45),
    needs: hex(0x895B0C),
    needs_bg: hex(0xFBF4E3),
    needs_line: hex(0xDEC793),
    work: hex(0x236C96),
    done: hex(0x276D49),
    dialog: hex(0xF8FCFA),
    scrim: [30, 35, 32, 64],
};

pub const BLOSSOM: Theme = Theme {
    name: "Blossom",
    dark: false,
    bg: hex(0xF8F0F4),
    panel: hex(0xF8F0F4),
    term: hex(0xFFFAFD),
    raised: hex(0xEEDFE8),
    border: hex(0xE5D2DF),
    border2: hex(0xD1B6C9),
    row_line: hex(0xF1E5ED),
    text: hex(0x3C2539),
    text2: hex(0x61465D),
    text3: hex(0x80637A),
    faint: hex(0xBCA2B4),
    sel: hex(0xEEDFE8),
    sel_ring: hex(0xD1B6C9),
    tc: hex(0x50334A),
    tc_on: hex(0x3C2539),
    primary_bg: hex(0x3C2539),
    primary_fg: hex(0xFFFAFD),
    on_accent: hex(0xFFFAFD),
    add: hex(0x367548),
    del: hex(0xB13A55),
    needs: hex(0x895B13),
    needs_bg: hex(0xFCF3E4),
    needs_line: hex(0xE2CAA0),
    work: hex(0x6650A4),
    done: hex(0x367548),
    dialog: hex(0xFFFAFD),
    scrim: [30, 35, 32, 64],
};

/// Dark schemes first, paired with light schemes in the same order for ⌘T.
pub const THEMES: [&Theme; 12] = [
    &GRUVBOX, &INK, &MOSS, &EMBER, &OCEAN, &PLUM,
    &GRUVBOX_LIGHT, &SNOW, &PAPER, &LINEN, &SEAFOAM, &BLOSSOM,
];

/// Saved scheme names, with legacy aliases and Gruvbox as the fallback.
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
        // gemini's own aliases, so they follow its releases.
        "gemini" => &[("default", ""), ("pro", "pro"), ("flash", "flash"), ("flash-lite", "flash-lite")],
        _ => &[("default", "")],
    }
}

/// Reasoning efforts the agent takes, as (shown, passed); 0 is its default.
pub fn efforts(agent: &str) -> &'static [(&'static str, &'static str)] {
    match agent {
        "claude" => &[("default", ""), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh"), ("max", "max")],
        "codex" => &[("default", ""), ("minimal", "minimal"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh")],
        "pi" => &[("default", ""), ("off", "off"), ("minimal", "minimal"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh")],
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

/// The agent's official brand mark as a filled SVG path in its 24-unit viewBox, drawn in its
/// brand colour. Sourced from simple-icons (Claude, Google Gemini, Pi, OpenAI), with every arc
/// command spelled out, since zeno drops an arc that relies on an implicit repeated command.
pub fn agent_logo(agent: &str) -> &'static str {
    match agent {
        "claude" => "m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z",
        "codex" => "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108a6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9a6.0462 6.0462 0 0 0 .7427 7.0966a5.98 5.98 0 0 0 .511 4.9107a6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058a5.9894 5.9894 0 0 0 3.9977-2.9001a6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804l4.7783-2.7582a.7948 .7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071 .071 0 0 1 .038 .052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142 .0852l4.783 2.7582a.7712 .7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804 .0804 0 0 1-.0332 .0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664 .7664 0 0 0 .3879 .6765l5.8144 3.3543l-2.0201 1.1685a.0757 .0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364L15.1192 7.2a.0757 .0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79 .79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852l-4.7735-2.7818a.7759 .7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662 .0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804 .0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142 .0805L8.704 5.459a.7948 .7948 0 0 0-.3927 .6813zm1.0976-2.3654l2.602-1.4998l2.6069 1.4998v2.9994l-2.5974 1.4997l-2.6067-1.4997Z",
        "pi" => "M0 0v24h6v-6h6v-6H6V6h6v6h6V0Zm18 12v12h6V12Z",
        "gemini" => "M11.04 19.32Q12 21.51 12 24q0-2.49 .93-4.68 .96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58a12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68 .96 2.19 .93 3.81 2.55t2.55 3.81",
        _ => "M12 2A10 10 0 1 0 12 22A10 10 0 1 0 12 2Z",
    }
}

impl Theme {
    /// GitHub's diff palette: (add sign, add row, del sign, del row), keyed on light/dark.
    pub fn diff(&self) -> (Rgba, Rgba, Rgba, Rgba) {
        if self.dark {
            (hex(0x3FB950), [46, 160, 67, 0x33], hex(0xF85149), [248, 81, 73, 0x26])
        } else {
            (hex(0x1A7F37), hex(0xDAFBE1), hex(0xCF222E), hex(0xFFEBE9))
        }
    }

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

#[cfg(test)]
mod tests {
    use super::agent_logo;
    use swash::zeno::{Fill, Mask};

    /// Every agent's mark rasterises to fill its 24-unit box, so a typo in the embedded brand
    /// path (or syntax zeno skips, like an arc's implicit repeated command) can't draw a sliver.
    #[test]
    fn every_agent_logo_rasterises() {
        for agent in ["claude", "codex", "pi", "gemini"] {
            let mut mask = Mask::new(agent_logo(agent));
            mask.style(Fill::NonZero);
            let (image, at) = mask.render();
            assert!(
                at.width >= 20 && at.height >= 20 && image.iter().any(|&p| p != 0),
                "{agent}'s logo doesn't fill its 24-unit box ({}x{})",
                at.width,
                at.height
            );
        }
    }
}

