mod elng;
mod hdlr;
mod mdhd;
mod minf;

pub use elng::*;
pub use hdlr::*;
pub use mdhd::*;
pub use minf::*;

use crate::*;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mdia {
    pub mdhd: Mdhd,
    pub hdlr: Hdlr,
    /// muxl vendor addition: optional ExtendedLanguageBox.
    pub elng: Option<Elng>,
    pub minf: Minf,
}

// Hand-written instead of `nested!` so `elng` is encoded in the ISO/IEC
// 14496-12 Table 1 position (mdhd, hdlr, elng, minf) rather than after the
// required boxes. Decoding is order-independent, matching `nested!`.
impl Atom for Mdia {
    const KIND: FourCC = FourCC::new(b"mdia");

    fn decode_body<B: Buf>(buf: &mut B) -> Result<Self> {
        let mut mdhd = None;
        let mut hdlr = None;
        let mut elng = None;
        let mut minf = None;

        while let Some(atom) = Any::decode_maybe(buf)? {
            match atom {
                Any::Mdhd(atom) => {
                    if mdhd.is_some() {
                        return Err(Error::DuplicateBox(Mdhd::KIND));
                    }
                    mdhd = Some(atom);
                }
                Any::Hdlr(atom) => {
                    if hdlr.is_some() {
                        return Err(Error::DuplicateBox(Hdlr::KIND));
                    }
                    hdlr = Some(atom);
                }
                Any::Elng(atom) => {
                    if elng.is_some() {
                        return Err(Error::DuplicateBox(Elng::KIND));
                    }
                    elng = Some(atom);
                }
                Any::Minf(atom) => {
                    if minf.is_some() {
                        return Err(Error::DuplicateBox(Minf::KIND));
                    }
                    minf = Some(atom);
                }
                Any::Skip(atom) => tracing::debug!(size = atom.zeroed.size, "skipping skip box"),
                Any::Free(atom) => tracing::debug!(size = atom.zeroed.size, "skipping free box"),
                unknown => Self::decode_unknown(&unknown)?,
            }
        }

        Ok(Self {
            mdhd: mdhd.ok_or(Error::MissingBox(Mdhd::KIND))?,
            hdlr: hdlr.ok_or(Error::MissingBox(Hdlr::KIND))?,
            elng,
            minf: minf.ok_or(Error::MissingBox(Minf::KIND))?,
        })
    }

    fn encode_body<B: BufMut>(&self, buf: &mut B) -> Result<()> {
        self.mdhd.encode(buf)?;
        self.hdlr.encode(buf)?;
        self.elng.encode(buf)?;
        self.minf.encode(buf)?;
        Ok(())
    }
}
