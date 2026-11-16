//! Pure parts of the subtitle check (spec 040): which tracks can be judged
//! ([`lang`]), which cues are sent to the aligner ([`probes`]) and how its
//! scores become a verdict per track ([`verdict`]). No I/O; the server's job
//! does the aligner calls and storage.

pub mod lang;
pub mod probes;
