//! The shader catalogue: every program the GPU layer may run, compiled into
//! the bundle. Themes and screens name a program; no shader source comes
//! from data (010: themes carry no shader code).

use crate::theme::value::ShaderName;

/// One allow-listed fragment shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Program {
    /// Mirrored spectrum bars rising from a reflective floor.
    Spectrum,
    /// A glowing oscilloscope trace of the waveform.
    Scope,
    /// A ring that swells with the spectrum around its circumference.
    Halo,
    /// A slow plasma field that the bass pushes along.
    Plasma,
    /// Backdrop: slow curtains of the accent along the top of the page.
    Aurora,
    /// Backdrop: fine scanlines with a rolling band, like a CRT.
    Scanlines,
    /// Backdrop: columns of glyphs falling in the accent.
    MatrixRain,
    /// Backdrop: moving film grain.
    Grain,
}

impl Program {
    /// The fragment shader body, joined after [`PRELUDE`].
    pub fn fragment(self) -> &'static str {
        match self {
            Program::Spectrum => include_str!("shaders/spectrum.frag"),
            Program::Scope => include_str!("shaders/scope.frag"),
            Program::Halo => include_str!("shaders/halo.frag"),
            Program::Plasma => include_str!("shaders/plasma.frag"),
            Program::Aurora => concat!(
                include_str!("shaders/noise.frag"),
                include_str!("shaders/aurora.frag")
            ),
            Program::Scanlines => concat!(
                include_str!("shaders/noise.frag"),
                include_str!("shaders/scanlines.frag")
            ),
            Program::MatrixRain => concat!(
                include_str!("shaders/noise.frag"),
                include_str!("shaders/matrix-rain.frag")
            ),
            Program::Grain => concat!(
                include_str!("shaders/noise.frag"),
                include_str!("shaders/grain.frag")
            ),
        }
    }

    /// The program that draws a theme's backdrop shader.
    pub fn backdrop(shader: ShaderName) -> Self {
        match shader {
            ShaderName::Aurora => Program::Aurora,
            ShaderName::Scanlines => Program::Scanlines,
            ShaderName::MatrixRain => Program::MatrixRain,
            ShaderName::Grain => Program::Grain,
        }
    }
}

/// A triangle covering the viewport, from `gl_VertexID` alone.
pub const VERTEX: &str = include_str!("shaders/fullscreen.vert");

/// Shared uniforms and samplers every fragment shader starts with.
pub const PRELUDE: &str = include_str!("shaders/prelude.frag");

/// The full fragment source for `program`.
pub fn fragment_source(program: Program) -> String {
    format!("{PRELUDE}\n{}", program.fragment())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Program; 8] = [
        Program::Spectrum,
        Program::Scope,
        Program::Halo,
        Program::Plasma,
        Program::Aurora,
        Program::Scanlines,
        Program::MatrixRain,
        Program::Grain,
    ];

    #[test]
    fn every_program_is_glsl_es_300_with_one_main() {
        assert!(VERTEX.starts_with("#version 300 es"));
        for program in ALL {
            let source = fragment_source(program);
            assert!(source.starts_with("#version 300 es"), "{program:?}");
            assert_eq!(source.matches("void main()").count(), 1, "{program:?}");
            assert!(!program.fragment().contains("#version"), "{program:?}");
        }
    }

    #[test]
    fn every_theme_shader_has_its_own_backdrop_program() {
        let programs: std::collections::HashSet<Program> = ShaderName::ALL
            .iter()
            .map(|s| Program::backdrop(*s))
            .collect();
        assert_eq!(programs.len(), ShaderName::ALL.len());
    }
}
