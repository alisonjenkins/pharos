//! htdemucs on the GPU: the network the flake converts to Burn
//! (`separatorModel`), with the weights it wrote beside it.

use std::path::Path;

use burn::tensor::{Tensor, TensorData};
use burn_store::{BurnpackStore, ModuleSnapshot};

use super::{Output, SeparateError, Stereo, CHANNELS, FRAMES, SEGMENT};
use crate::device::{burn_device, Adapter};

#[allow(
    dead_code,
    unused_imports,
    clippy::all,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod generated {
    include!(concat!(env!("PHAROS_SEPARATOR_MODEL"), "/htdemucs.rs"));
}

/// The weights file's length when this binary was built: a different one is
/// another conversion, or truncated.
const BPK_LEN: &str = env!("PHAROS_SEPARATOR_BPK_LEN");

/// The weights next to the generated network in the flake's model output.
pub const BPK: &str = concat!(env!("PHAROS_SEPARATOR_MODEL"), "/htdemucs.bpk");

pub struct Separator {
    model: generated::Model,
    device: burn::tensor::Device,
}

impl Separator {
    /// Uploads the weights in `bpk` to `adapter`.
    pub fn load(bpk: &Path, adapter: &Adapter) -> Result<Self, SeparateError> {
        let fail = |message: String| SeparateError::Model {
            path: bpk.to_path_buf(),
            message,
        };
        let len = std::fs::metadata(bpk)
            .map_err(|e| fail(e.to_string()))?
            .len();
        if len.to_string() != BPK_LEN {
            return Err(fail(format!("{len} bytes, built for {BPK_LEN}")));
        }
        let device = burn_device(adapter);
        let mut model = generated::Model::new(&device);
        let applied = model
            .load_from(&mut BurnpackStore::from_file(bpk))
            .map_err(|e| fail(format!("{e:?}")))?;
        if !applied.errors.is_empty() || !applied.missing.is_empty() {
            return Err(fail(format!(
                "{} errors, {} tensors missing: {:?} {:?}",
                applied.errors.len(),
                applied.missing.len(),
                applied.errors.first(),
                applied.missing.first()
            )));
        }
        Ok(Self { model, device })
    }

    /// One segment through the network, in [`super::vocals`]'s layout.
    fn segment(&self, mix: &[f32], mag: &[f32]) -> Result<Output, String> {
        let mix = Tensor::<3>::from_data(
            TensorData::new(mix.to_vec(), [1, CHANNELS, SEGMENT]),
            &self.device,
        );
        let mag = Tensor::<4>::from_data(
            TensorData::new(mag.to_vec(), [1, 2 * CHANNELS, super::BINS, FRAMES]),
            &self.device,
        );
        let (x, xt) = self.model.forward(mix, mag);
        let read = |what: &str, d: TensorData| {
            d.try_to_vec::<f32>()
                .map_err(|e| format!("reading {what} back from the GPU: {e:?}"))
        };
        Ok((read("x", x.into_data())?, read("xt", xt.into_data())?))
    }

    /// The vocals of a whole song at [`super::RATE`].
    pub fn vocals(&self, mix: &Stereo) -> Result<Stereo, SeparateError> {
        super::vocals(mix, |m, s| self.segment(m, s))
    }

    /// Drops the weights and hands the device pool's freed pages back: the
    /// letter model shares the pool, so dropping alone keeps them reserved.
    pub fn release(self) {
        let Self { model, device } = self;
        drop(model);
        device.memory_cleanup();
    }
}
