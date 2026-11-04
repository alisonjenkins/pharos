//! Reader settings and the stylesheet that applies them over a book's own.

/// Which typeface the text is set in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Font {
    /// The book's own `font-family` rules (embedded fonts are not loaded, so
    /// this falls back to the families they name and then the generic one).
    #[default]
    Book,
    Serif,
    Sans,
}

/// Colours as CSS values (`#1b1b1b`, `rgb(...)`, `var(--x)` from the host's
/// page, which custom properties inherit through the shadow boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub text: String,
    pub background: String,
    pub link: String,
}

impl Theme {
    pub fn paper() -> Self {
        Self::of("#1b1b1b", "#fdfdfb", "#1d4ed8")
    }

    pub fn sepia() -> Self {
        Self::of("#3b2f22", "#f4ecd8", "#7c4a12")
    }

    pub fn night() -> Self {
        Self::of("#d6d3cd", "#121212", "#8ab4f8")
    }

    fn of(text: &str, background: &str, link: &str) -> Self {
        Self {
            text: text.into(),
            background: background.into(),
            link: link.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub font_size_px: f64,
    /// Unitless `line-height`.
    pub line_height: f64,
    pub font: Font,
    /// Space around the text on every side, and half the gap between pages.
    pub margin_px: f64,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size_px: 18.0,
            line_height: 1.5,
            font: Font::Book,
            margin_px: 40.0,
            theme: Theme::paper(),
        }
    }
}

impl Settings {
    /// The view's own stylesheet for a page `width` x `height` with `gap`
    /// between pages. Placed after the book's sheets; `!important` where the
    /// reader's choice must win over the book's.
    pub(crate) fn css(&self, width: f64, gap: f64, height: f64) -> String {
        let font = match self.font {
            Font::Book => String::new(),
            Font::Serif => family("Charter, 'Iowan Old Style', Georgia, 'Times New Roman', serif"),
            Font::Sans => family("system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif"),
        };
        let Theme {
            text,
            background,
            link,
        } = &self.theme;
        let (text, background, link) = (clean(text), clean(background), clean(link));
        let m = self.margin_px;
        format!(
            ":host{{display:block;position:relative;overflow:hidden;background:{background};}}\
             .quire-sheets{{display:none;}}\
             .quire-frame{{position:absolute;inset:0;padding:{m}px;box-sizing:border-box;overflow:hidden;}}\
             .quire-strip{{width:{width}px;height:100%;column-width:{width}px;column-gap:{gap}px;\
             column-fill:auto;overflow:visible;will-change:transform;\
             font-size:{size}px !important;line-height:{lh} !important;color:{text} !important;\
             overflow-wrap:break-word;-webkit-user-select:text;user-select:text;}}\
             .quire-strip *{{color:inherit !important;background-color:transparent !important;\
             border-color:currentColor !important;max-width:100%;line-height:inherit !important;}}\
             .quire-strip :is(p,li,dd,dt,blockquote,td,th,div){{\
             font-size:{size}px !important;}}\
             .quire-strip :is(sub,sup,small){{font-size:0.75em !important;}}\
             .quire-strip a{{color:{link} !important;}}\
             .quire-strip [data-quire-link]{{cursor:pointer;text-decoration:underline;}}\
             .quire-strip img,.quire-strip svg{{max-height:{height}px;height:auto;object-fit:contain;\
             break-inside:avoid;}}\
             .quire-strip pre{{white-space:pre-wrap;}}\
             .quire-strip table{{max-width:100%;}}\
             {font}",
            size = self.font_size_px,
            lh = self.line_height,
        )
    }
}

fn family(f: &str) -> String {
    format!(".quire-strip,.quire-strip *{{font-family:{f} !important;}}")
}

/// A colour may not carry syntax that ends its declaration.
fn clean(v: &str) -> String {
    v.chars()
        .filter(|c| !matches!(c, ';' | '{' | '}' | '<' | '>' | '\\'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_cannot_break_out_of_its_rule() {
        let mut s = Settings::default();
        s.theme.text = "red;}*{display:none".into();
        let css = s.css(600.0, 80.0, 800.0);
        assert!(css.contains("color:red*display:none !important"));
        assert!(!css.contains("red;}"));
    }

    #[test]
    fn the_book_font_adds_no_family_rule() {
        let mut s = Settings::default();
        assert!(!s.css(1.0, 1.0, 1.0).contains("font-family"));
        s.font = Font::Serif;
        assert!(s.css(1.0, 1.0, 1.0).contains("font-family:Charter"));
    }
}
