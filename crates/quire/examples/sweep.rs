//! Opens each EPUB named on the command line and sanitises every section,
//! printing one line per book: `ok <sections> <chars> <path>` or `FAIL <why>`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut failed = false;
    for path in std::env::args().skip(1) {
        match sweep(&path) {
            Ok((sections, chars)) => println!("ok {sections} {chars} {path}"),
            Err(e) => {
                failed = true;
                println!("FAIL {path}: {e}");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn sweep(path: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let book = quire::Publication::open_epub(std::fs::read(path)?)?;
    let mut chars = 0usize;
    for i in 0..book.spine().len() {
        let section = book.section(i).map_err(|e| format!("section {i}: {e}"))?;
        chars = chars.saturating_add(section.text_len_utf16());
        for r in section.resources() {
            book.resource(&r)
                .map_err(|e| format!("section {i} resource: {e}"))?;
        }
    }
    Ok((book.spine().len(), chars))
}
