use nanocode_tui::ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub fn lines(text: &str) -> Vec<Line<'static>> {
    let mut style = Style::default();
    text.lines()
        .map(|line| {
            let mut spans = Vec::new();
            let mut current = String::new();
            let mut rest = line;
            while let Some(start) = rest.find('\u{1b}') {
                current.push_str(&rest[..start]);
                let sequence = &rest[start + 1..];
                let Some(body) = sequence.strip_prefix('[') else {
                    rest = sequence;
                    continue;
                };
                let end = body
                    .find(|character: char| ('@'..='~').contains(&character))
                    .unwrap_or(body.len());
                if body[end..].starts_with('m') {
                    if !current.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut current), style));
                    }
                    style = apply(style, &body[..end]);
                }
                rest = body.get(end + 1..).unwrap_or("");
            }
            current.push_str(rest);
            if !current.is_empty() {
                spans.push(Span::styled(current, style));
            }
            Line::from(spans)
        })
        .collect()
}

fn apply(mut style: Style, parameters: &str) -> Style {
    let codes = parameters
        .split(';')
        .map(|code| code.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>();
    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            code @ 30..=37 => style = style.fg(Color::Indexed((code - 30) as u8)),
            code @ 90..=97 => style = style.fg(Color::Indexed((code - 90 + 8) as u8)),
            code @ 40..=47 => style = style.bg(Color::Indexed((code - 40) as u8)),
            code @ 100..=107 => style = style.bg(Color::Indexed((code - 100 + 8) as u8)),
            39 => style.fg = None,
            49 => style.bg = None,
            code @ (38 | 48) => {
                let (color, used) = extended(&codes[index + 1..]);
                if let Some(color) = color {
                    style = if code == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
                index += used;
            }
            _ => {}
        }
        index += 1;
    }
    style
}

fn extended(codes: &[u16]) -> (Option<Color>, usize) {
    let byte = |index: usize| codes.get(index).and_then(|code| u8::try_from(*code).ok());
    match codes.first() {
        Some(5) => (byte(1).map(Color::Indexed), 2),
        Some(2) => match (byte(1), byte(2), byte(3)) {
            (Some(red), Some(green), Some(blue)) => (Some(Color::Rgb(red, green, blue)), 4),
            _ => (None, codes.len()),
        },
        _ => (None, 0),
    }
}
